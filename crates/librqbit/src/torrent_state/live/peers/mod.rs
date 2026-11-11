use std::{collections::HashSet, net::SocketAddr, sync::Arc};

use dashmap::DashMap;
use librqbit_core::lengths::ValidPieceIndex;
use parking_lot::RwLock;

use crate::{
    Error,
    piece_picker::PieceAvailability,
    torrent_state::utils::{TimedExistence, atomic_inc},
    type_aliases::{BF, PeerHandle},
};

use self::stats::{AggregatePeerStats, AggregatePeerStatsAtomic};

use super::peer::{LivePeerState, Peer, PeerRx, PeerState, PeerTx};

pub mod stats;

pub(crate) struct PeerStates {
    pub session_stats: Arc<AggregatePeerStatsAtomic>,

    // This keeps track of live addresses we connected to, for PEX.
    pub live_outgoing_peers: RwLock<HashSet<PeerHandle>>,
    pub stats: AggregatePeerStatsAtomic,
    pub states: DashMap<PeerHandle, Peer>,
    // How many live peers have each piece, kept in sync with their bitfields: counted when a
    // bitfield arrives or grows, forgotten when the peer leaves the live state.
    pub availability: PieceAvailability,
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
        if let PeerState::Live(l) = s {
            self.forget_availability(l);
        }

        Some(p)
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

    pub fn update_bitfield(
        &self,
        handle: PeerHandle,
        bitfield: BF,
        total_pieces: usize,
    ) -> Option<()> {
        self.with_live_mut(handle, "update_bitfield", |live| {
            self.forget_availability(live);
            live.bitfield = bitfield;
            self.count_availability(live, total_pieces);
            self.update_seeder_flag(live, total_pieces);
        })
    }

    fn count_availability(&self, live: &mut LivePeerState, total_pieces: usize) {
        live.counted_as_seed = live.has_full_torrent(total_pieces);
        if live.counted_as_seed {
            self.availability.add_seed();
        } else {
            self.availability.add_pieces(&live.bitfield);
        }
    }

    /// Undo what this live peer's bitfield added to the availability.
    pub(crate) fn forget_availability(&self, live: &LivePeerState) {
        if live.counted_as_seed {
            self.availability.remove_seed();
        } else {
            self.availability.remove_pieces(&live.bitfield);
        }
    }

    /// The peer announced `piece`, which its bitfield didn't have.
    pub fn on_new_peer_piece(&self, live: &LivePeerState, piece: usize) {
        // A seed already counts for every piece.
        if !live.counted_as_seed {
            self.availability.add_piece(piece);
        }
    }

    // Recompute the cached "this peer has the full torrent" flag after its bitfield
    // changed, keeping the aggregate seeder counters in sync. Must be called on every
    // bitfield change of a live peer.
    pub fn update_seeder_flag(&self, live: &mut LivePeerState, total_pieces: usize) {
        let seeder = live.has_full_torrent(total_pieces);
        if seeder == live.seeder {
            return;
        }
        live.seeder = seeder;
        self.stats.seeder_flag_changed(seeder);
        self.session_stats.seeder_flag_changed(seeder);
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
    use librqbit_core::hash_id::Id20;

    use super::*;
    use crate::stream_connect::ConnectionKind;

    const TOTAL_PIECES: usize = 8;

    fn full_bitfield() -> BF {
        BF::from_boxed_slice(vec![0xffu8; 1].into_boxed_slice())
    }

    fn empty_bitfield() -> BF {
        BF::from_boxed_slice(vec![0u8; 1].into_boxed_slice())
    }

    fn make_peer_states() -> PeerStates {
        PeerStates {
            session_stats: Default::default(),
            live_outgoing_peers: Default::default(),
            stats: Default::default(),
            states: Default::default(),
            availability: PieceAvailability::new(u32::try_from(TOTAL_PIECES).unwrap()),
        }
    }

    // Returns the peer handle, keeping the receiving end of the peer channel alive.
    fn add_live_peer(peers: &PeerStates, addr: &str) -> (PeerHandle, PeerRx) {
        let addr: SocketAddr = addr.parse().unwrap();
        peers.add_if_not_seen(addr).unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        peers
            .with_peer_mut(addr, "test", |peer| {
                peer.incoming_connection(Id20::default(), tx, peers, ConnectionKind::Tcp)
            })
            .unwrap()
            .unwrap();
        (addr, rx)
    }

    fn live_seeders(peers: &PeerStates) -> (u32, u32) {
        (
            peers.stats().live_seeders,
            peers.session_stats.snapshot().live_seeders,
        )
    }

    #[test]
    fn full_bitfield_counts_as_seeder_and_is_uncounted_on_disconnect() {
        let peers = make_peer_states();
        let (addr, _rx) = add_live_peer(&peers, "127.0.0.1:1234");
        assert_eq!(live_seeders(&peers), (0, 0));

        peers
            .update_bitfield(addr, empty_bitfield(), TOTAL_PIECES)
            .unwrap();
        assert_eq!(live_seeders(&peers), (0, 0));

        peers
            .update_bitfield(addr, full_bitfield(), TOTAL_PIECES)
            .unwrap();
        assert_eq!(live_seeders(&peers), (1, 1));

        // Repeated updates must not double-count.
        peers
            .update_bitfield(addr, full_bitfield(), TOTAL_PIECES)
            .unwrap();
        assert_eq!(live_seeders(&peers), (1, 1));

        peers.drop_peer(addr).unwrap();
        assert_eq!(live_seeders(&peers), (0, 0));
    }

    #[test]
    fn bitfield_completed_piece_by_piece_counts_as_seeder() {
        let peers = make_peer_states();
        let (addr, _rx) = add_live_peer(&peers, "127.0.0.1:1234");
        peers
            .update_bitfield(addr, empty_bitfield(), TOTAL_PIECES)
            .unwrap();

        for piece in 0..TOTAL_PIECES {
            peers
                .with_live_mut(addr, "test", |live| {
                    // As on_have does.
                    *live.bitfield.get_mut(piece).unwrap() = true;
                    peers.on_new_peer_piece(live, piece);
                    peers.update_seeder_flag(live, TOTAL_PIECES);
                })
                .unwrap();
            let expected = u32::from(piece == TOTAL_PIECES - 1);
            assert_eq!(live_seeders(&peers), (expected, expected));
        }
    }

    #[test]
    fn seeder_is_uncounted_when_peer_leaves_live_state() {
        let peers = make_peer_states();
        let (addr, _rx) = add_live_peer(&peers, "127.0.0.1:1234");
        peers
            .update_bitfield(addr, full_bitfield(), TOTAL_PIECES)
            .unwrap();
        assert_eq!(live_seeders(&peers), (1, 1));

        peers.mark_peer_not_needed(addr).unwrap();
        assert_eq!(live_seeders(&peers), (0, 0));
    }

    #[test]
    fn seeders_are_counted_per_peer() {
        let peers = make_peer_states();
        let (a, _rx_a) = add_live_peer(&peers, "127.0.0.1:1234");
        let (b, _rx_b) = add_live_peer(&peers, "127.0.0.1:1235");

        peers
            .update_bitfield(a, full_bitfield(), TOTAL_PIECES)
            .unwrap();
        peers
            .update_bitfield(b, empty_bitfield(), TOTAL_PIECES)
            .unwrap();
        assert_eq!(live_seeders(&peers), (1, 1));

        peers
            .update_bitfield(b, full_bitfield(), TOTAL_PIECES)
            .unwrap();
        assert_eq!(live_seeders(&peers), (2, 2));

        peers.drop_peer(a).unwrap();
        assert_eq!(live_seeders(&peers), (1, 1));
    }

    fn availability(peers: &PeerStates) -> Vec<u32> {
        (0..TOTAL_PIECES)
            .map(|p| peers.availability.get(p))
            .collect()
    }

    fn bitfield(bits: u8) -> BF {
        BF::from_boxed_slice(vec![bits].into_boxed_slice())
    }

    #[test]
    fn availability_follows_bitfields_and_haves() {
        let peers = make_peer_states();
        let (a, _rx_a) = add_live_peer(&peers, "127.0.0.1:1234");
        let (b, _rx_b) = add_live_peer(&peers, "127.0.0.1:1235");

        peers
            .update_bitfield(a, bitfield(0b1100_0000), TOTAL_PIECES)
            .unwrap();
        peers
            .update_bitfield(b, bitfield(0b1000_0001), TOTAL_PIECES)
            .unwrap();
        assert_eq!(availability(&peers), vec![2, 1, 0, 0, 0, 0, 0, 1]);

        // A have adds the piece; a second bitfield replaces the first one.
        peers
            .with_live_mut(a, "test", |live| {
                *live.bitfield.get_mut(4).unwrap() = true;
                peers.on_new_peer_piece(live, 4);
            })
            .unwrap();
        peers
            .update_bitfield(b, bitfield(0b0000_0001), TOTAL_PIECES)
            .unwrap();
        assert_eq!(availability(&peers), vec![1, 1, 0, 0, 1, 0, 0, 1]);

        // Leaving the live state takes the peer's pieces with it, whichever way it leaves.
        peers.mark_peer_not_needed(a).unwrap();
        assert_eq!(availability(&peers), vec![0, 0, 0, 0, 0, 0, 0, 1]);
        peers.drop_peer(b).unwrap();
        assert_eq!(availability(&peers), vec![0; TOTAL_PIECES]);
    }

    #[test]
    fn seed_availability_is_counted_once_and_undone_as_a_seed() {
        let peers = make_peer_states();
        let (a, _rx_a) = add_live_peer(&peers, "127.0.0.1:1234");
        let (b, _rx_b) = add_live_peer(&peers, "127.0.0.1:1235");
        peers
            .update_bitfield(a, full_bitfield(), TOTAL_PIECES)
            .unwrap();
        peers
            .update_bitfield(b, bitfield(0b0111_1111), TOTAL_PIECES)
            .unwrap();
        assert_eq!(availability(&peers), vec![1, 2, 2, 2, 2, 2, 2, 2]);

        // b completes through a have: still counted piece by piece, so still undone that way.
        peers
            .with_live_mut(b, "test", |live| {
                *live.bitfield.get_mut(0).unwrap() = true;
                peers.on_new_peer_piece(live, 0);
            })
            .unwrap();
        assert_eq!(availability(&peers), vec![2; TOTAL_PIECES]);
        peers.drop_peer(b).unwrap();
        assert_eq!(availability(&peers), vec![1; TOTAL_PIECES]);
        peers.drop_peer(a).unwrap();
        assert_eq!(availability(&peers), vec![0; TOTAL_PIECES]);
    }
}
