use std::{collections::HashSet, net::SocketAddr, sync::Arc};

use dashmap::DashMap;
use librqbit_core::lengths::ValidPieceIndex;
use parking_lot::RwLock;
use tracing::debug;

use crate::{
    Error,
    torrent_state::utils::{TimedExistence, atomic_inc},
    type_aliases::{BF, PeerHandle},
};

use self::stats::{AggregatePeerStats, AggregatePeerStatsAtomic};

use super::peer::{LivePeerState, Peer, PeerRx, PeerState, PeerTx};

pub mod stats;

/// Upper bound on how many peers we track per torrent before evicting the
/// ones that can never be used again.
///
/// Every incoming connection (plus every address learned from trackers/DHT)
/// creates an entry in [`PeerStates::states`]. Entries of peers that died or
/// are no longer needed are useless: incoming peers are not retried and
/// usually have ephemeral source ports, so their entries are dead weight.
/// On torrents with a lot of peer churn this map used to grow without a
/// bound, leaking memory
/// (https://github.com/ikatson/rqbit/issues/525).
pub(crate) const MAX_TRACKED_PEERS: usize = 1024;

pub(crate) struct PeerStates {
    pub session_stats: Arc<AggregatePeerStatsAtomic>,

    // This keeps track of live addresses we connected to, for PEX.
    pub live_outgoing_peers: RwLock<HashSet<PeerHandle>>,
    pub stats: AggregatePeerStatsAtomic,
    pub states: DashMap<PeerHandle, Peer>,
}

impl Drop for PeerStates {
    fn drop(&mut self) {
        for (_, p) in std::mem::take(&mut self.states).into_iter() {
            p.destroy(self);
        }
    }
}

impl PeerStates {
    pub fn stats(&self) -> AggregatePeerStats {
        self.stats.snapshot()
    }

    pub fn add_if_not_seen(&self, addr: SocketAddr) -> Option<PeerHandle> {
        use dashmap::mapref::entry::Entry;
        // Keep the peers map bounded: peers learned from trackers/DHT/PEX
        // accumulate here too, even for torrents that don't accept incoming
        // connections (https://github.com/ikatson/rqbit/issues/525).
        if self.states.len() >= MAX_TRACKED_PEERS {
            self.prune_useless_peers(MAX_TRACKED_PEERS - MAX_TRACKED_PEERS / 4);
        }
        match self.states.entry(addr) {
            Entry::Occupied(_) => None,
            Entry::Vacant(vac) => {
                vac.insert(Peer::new_with_outgoing_address(addr));
                atomic_inc(&self.stats.queued);
                atomic_inc(&self.session_stats.queued);

                atomic_inc(&self.stats.seen);
                atomic_inc(&self.session_stats.seen);
                Some(addr)
            }
        }
    }
    pub fn with_peer<R>(&self, addr: PeerHandle, f: impl FnOnce(&Peer) -> R) -> Option<R> {
        self.states.get(&addr).map(|e| f(e.value()))
    }

    pub fn with_peer_mut<R>(
        &self,
        addr: PeerHandle,
        reason: &'static str,
        f: impl FnOnce(&mut Peer) -> R,
    ) -> Option<R> {
        use crate::torrent_state::utils::timeit;
        timeit(reason, || self.states.get_mut(&addr))
            .map(|e| f(TimedExistence::new(e, reason).value_mut()))
    }

    pub fn with_live<R>(&self, addr: PeerHandle, f: impl FnOnce(&LivePeerState) -> R) -> Option<R> {
        self.with_peer(addr, |peer| peer.get_live().map(f))
            .flatten()
    }

    pub fn with_live_mut<R>(
        &self,
        addr: PeerHandle,
        reason: &'static str,
        f: impl FnOnce(&mut LivePeerState) -> R,
    ) -> Option<R> {
        self.with_peer_mut(addr, reason, |peer| peer.get_live_mut().map(f))
            .flatten()
    }

    pub fn drop_peer(&self, handle: PeerHandle) -> Option<Peer> {
        let p = self.states.remove(&handle).map(|r| r.1)?;
        let s = p.get_state();
        self.stats.dec(s);
        self.session_stats.dec(s);

        Some(p)
    }

    /// Removes the peer if its state satisfies `cond`, keeping the aggregate
    /// stats counters consistent. Unlike [`PeerStates::drop_peer`], the
    /// removal is conditional, so a peer that concurrently became alive again
    /// (e.g. reconnected) is left untouched.
    fn drop_peer_if(&self, handle: PeerHandle, cond: impl Fn(&Peer) -> bool) -> Option<Peer> {
        let p = self
            .states
            .remove_if(&handle, |_, p| cond(p))
            .map(|(_, p)| p)?;
        let s = p.get_state();
        self.stats.dec(s);
        self.session_stats.dec(s);

        Some(p)
    }

    /// Evicts peer entries that can never be used again, until at most
    /// `max_keep` peers are tracked. Returns how many entries were removed.
    ///
    /// Only entries that are useless no matter what are evicted, in this
    /// order:
    ///
    /// 1. `NotNeeded` peers we have no outgoing address for (i.e. peers that
    ///    connected to us). They are never retried and usually have ephemeral
    ///    ports, so there is no way to ever talk to them again.
    /// 2. `Dead` peers we have no outgoing address for. Incoming peers are
    ///    never re-queued.
    /// 3. `NotNeeded` peers we could reconnect to ourselves. They are
    ///    revivable (e.g. when the selected files change), but re-learnable
    ///    from trackers/DHT, so evicting them is only a last resort to keep
    ///    this map bounded.
    ///
    /// Live, connecting, queued and dead outgoing peers are never touched.
    pub(crate) fn prune_useless_peers(&self, max_keep: usize) -> usize {
        let total = self.states.len();
        if total <= max_keep {
            return 0;
        }
        let mut to_remove = total - max_keep;
        let mut removed = 0;

        let evict_passes: [fn(&Peer) -> bool; 3] = [
            |p: &Peer| {
                matches!(p.get_state(), PeerState::NotNeeded) && p.outgoing_address.is_none()
            },
            |p: &Peer| matches!(p.get_state(), PeerState::Dead) && p.outgoing_address.is_none(),
            |p: &Peer| matches!(p.get_state(), PeerState::NotNeeded),
        ];

        for is_evictable in evict_passes {
            if to_remove == 0 {
                break;
            }
            let candidates: Vec<PeerHandle> = self
                .states
                .iter()
                .filter(|e| is_evictable(e.value()))
                .take(to_remove)
                .map(|e| *e.key())
                .collect();
            for handle in candidates {
                // remove_if re-checks the state, so peers that concurrently
                // became alive again are not dropped.
                if self.drop_peer_if(handle, is_evictable).is_some() {
                    removed += 1;
                    to_remove -= 1;
                    if to_remove == 0 {
                        break;
                    }
                }
            }
        }

        if removed > 0 {
            debug!(
                removed,
                remaining = self.states.len(),
                "pruned useless peers"
            );
        }
        removed
    }

    pub fn is_peer_not_interested_and_has_full_torrent(
        &self,
        handle: PeerHandle,
        total_pieces: usize,
    ) -> bool {
        self.with_live(handle, |live| {
            !live.peer_interested && live.has_full_torrent(total_pieces)
        })
        .unwrap_or(false)
    }

    pub fn mark_peer_interested(&self, handle: PeerHandle, is_interested: bool) -> Option<bool> {
        self.with_live_mut(handle, "mark_peer_interested", |live| {
            let prev = live.peer_interested;
            live.peer_interested = is_interested;
            prev
        })
    }

    pub fn update_bitfield(&self, handle: PeerHandle, bitfield: BF) -> Option<()> {
        self.with_live_mut(handle, "update_bitfield", |live| {
            live.bitfield = bitfield;
        })
    }

    pub fn mark_peer_connecting(&self, h: PeerHandle) -> crate::Result<(PeerRx, PeerTx)> {
        let rx = self
            .with_peer_mut(h, "mark_peer_connecting", |peer| {
                peer.idle_to_connecting(self)
                    .ok_or(Error::BugInvalidPeerState)
            })
            .ok_or(Error::BugPeerNotFound)??;
        Ok(rx)
    }

    pub fn reset_peer_backoff(&self, handle: PeerHandle) {
        self.with_peer_mut(handle, "reset_peer_backoff", |p| {
            p.stats.reset_backoff();
        });
    }

    pub fn mark_peer_not_needed(&self, handle: PeerHandle) -> Option<PeerState> {
        let prev = self.with_peer_mut(handle, "mark_peer_not_needed", |peer| {
            peer.set_not_needed(self)
        })?;
        Some(prev)
    }

    pub(crate) fn on_steal(
        &self,
        from_peer: SocketAddr,
        to_peer: SocketAddr,
        stolen_idx: ValidPieceIndex,
    ) {
        self.with_peer(to_peer, |p| {
            atomic_inc(&p.stats.counters.times_i_stole);
        });
        self.with_peer(from_peer, |p| {
            atomic_inc(&p.stats.counters.times_stolen_from_me);
        });
        self.stats.inc_steals();
        self.session_stats.inc_steals();

        self.with_live_mut(from_peer, "send_cancellations", |live| {
            live.cancel_inflight_requests_for_piece(stolen_idx);
        });
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use librqbit_core::hash_id::Id20;
    use tokio::sync::mpsc::unbounded_channel;

    use super::*;
    use crate::stream_connect::ConnectionKind;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    fn peer_states() -> PeerStates {
        PeerStates {
            session_stats: Default::default(),
            stats: Default::default(),
            states: Default::default(),
            live_outgoing_peers: Default::default(),
        }
    }

    fn peer_id() -> Id20 {
        Id20::new(*b"01234567890123456789")
    }

    /// Inserts a peer that looks like it connected to us (no outgoing
    /// address), in the given state. `None` means keep the initial Live
    /// state.
    fn insert_incoming(states: &PeerStates, port: u16, state: Option<PeerState>) {
        let (tx, _rx) = unbounded_channel();
        let mut peer = Peer::new_live_for_incoming_connection(
            addr(port),
            peer_id(),
            tx,
            states,
            ConnectionKind::Tcp,
        );
        if let Some(state) = state {
            peer.set_state(state, states);
        }
        states.states.insert(addr(port), peer);
    }

    /// Inserts a peer that looks like we connected to it ourselves (has an
    /// outgoing address), in the given state. Mirrors what `add_if_not_seen`
    /// does: the entry starts as queued (counted), then transitions.
    fn insert_outgoing(states: &PeerStates, port: u16, state: PeerState) {
        let mut peer = Peer::new_with_outgoing_address(addr(port));
        peer.set_state(state, states);
        states.states.insert(addr(port), peer);
        atomic_inc(&states.stats.queued);
        atomic_inc(&states.session_stats.queued);
    }

    fn state_of(states: &PeerStates, port: u16) -> Option<&'static str> {
        states.with_peer(addr(port), |p| p.get_state().name())
    }

    #[test]
    fn test_prune_useless_peers_noop_when_only_useful_entries() {
        let states = peer_states();
        insert_incoming(&states, 1, None);
        insert_outgoing(&states, 2, PeerState::Queued);
        // Live and queued peers are never evictable, even when asking to
        // keep less than we track.
        assert_eq!(states.prune_useless_peers(2), 0);
        assert_eq!(states.states.len(), 2);
        assert_eq!(states.prune_useless_peers(1), 0);
        assert_eq!(states.states.len(), 2);
    }

    #[test]
    fn test_prune_useless_peers_evicts_in_priority_order() {
        let states = peer_states();

        // 4 not-needed incoming, 3 dead incoming: fully removable.
        for port in 1..=4 {
            insert_incoming(&states, port, Some(PeerState::NotNeeded));
        }
        for port in 5..=7 {
            insert_incoming(&states, port, Some(PeerState::Dead));
        }
        // 2 not-needed outgoing: removable only as a last resort.
        for port in 8..=9 {
            insert_outgoing(&states, port, PeerState::NotNeeded);
        }
        // 2 dead outgoing: never removed.
        for port in 10..=11 {
            insert_outgoing(&states, port, PeerState::Dead);
        }
        // Live incoming and a queued outgoing: never removed.
        insert_incoming(&states, 12, None);
        insert_incoming(&states, 13, None);
        insert_outgoing(&states, 14, PeerState::Queued);

        // 14 total, ask to keep 8: 6 removed — all 4 not-needed incoming,
        // then 2 of the 3 dead incoming (dashmap iteration order is
        // arbitrary, so exactly which dead one survives is not guaranteed).
        assert_eq!(states.prune_useless_peers(8), 6);
        assert_eq!(states.states.len(), 8);

        for port in 1..=4 {
            assert_eq!(state_of(&states, port), None, "port {port} should be gone");
        }
        assert_eq!(
            (5..=7).filter(|p| state_of(&states, *p).is_none()).count(),
            2,
            "exactly 2 of the 3 dead incoming peers should be evicted"
        );
        for port in 8..=14 {
            assert!(state_of(&states, port).is_some(), "port {port} should stay");
        }

        // Counters must stay consistent with the remaining entries.
        let snapshot = states.stats.snapshot();
        assert_eq!(snapshot.not_needed, 2);
        assert_eq!(snapshot.dead, 3);
        assert_eq!(snapshot.live, 2);
        assert_eq!(snapshot.queued, 1);
    }

    #[test]
    fn test_prune_useless_peers_not_needed_outgoing_is_last_resort() {
        let states = peer_states();
        for port in 1..=4 {
            insert_outgoing(&states, port, PeerState::NotNeeded);
        }
        // Everything else is revivable, but still evicted to keep the map
        // bounded.
        assert_eq!(states.prune_useless_peers(2), 2);
        assert_eq!(states.states.len(), 2);
        assert_eq!(states.stats.snapshot().not_needed, 2);
    }

    #[test]
    fn test_prune_useless_peers_never_touches_live_or_dead_outgoing() {
        let states = peer_states();
        insert_incoming(&states, 1, None);
        insert_incoming(&states, 2, None);
        for port in 3..=6 {
            insert_outgoing(&states, port, PeerState::Dead);
        }
        // Only live and dead-outgoing peers: nothing is evictable even with
        // max_keep = 0.
        assert_eq!(states.prune_useless_peers(0), 0);
        assert_eq!(states.states.len(), 6);
        assert_eq!(states.stats.snapshot().live, 2);
        assert_eq!(states.stats.snapshot().dead, 4);
    }

    #[test]
    fn test_add_if_not_seen_prunes_useless_peers() {
        let states = peer_states();

        // Simulate a torrent that was fed many tracker/DHT peers which then
        // all became not needed (e.g. because it's finished and upload is
        // disabled).
        let mut addrs = Vec::new();
        for i in 0..1500u16 {
            let addr = addr(3000 + i);
            assert!(states.add_if_not_seen(addr).is_some());
            addrs.push(addr);
        }
        assert_eq!(states.states.len(), 1500);
        for addr in &addrs {
            states.mark_peer_not_needed(*addr);
        }
        assert_eq!(states.stats.snapshot().not_needed, 1500);

        // Adding more peers has to trigger the eviction: all the not-needed
        // entries get evicted to make room, while the freshly added ones
        // (queued, useful) stay.
        for i in 0..1500u16 {
            assert!(states.add_if_not_seen(addr(6000 + i)).is_some());
        }
        let snapshot = states.stats.snapshot();
        assert_eq!(
            snapshot.not_needed, 0,
            "all not-needed entries should have been pruned"
        );
        assert_eq!(
            snapshot.queued as usize,
            states.states.len(),
            "remaining entries should be the freshly queued ones"
        );
    }
}
