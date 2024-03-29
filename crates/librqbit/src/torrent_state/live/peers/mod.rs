use std::net::SocketAddr;

use anyhow::Context;
use backoff::backoff::Backoff;
use dashmap::DashMap;

use crate::{
    torrent_state::utils::{atomic_inc, TimedExistence},
    type_aliases::{PeerHandle, BF},
};

use self::stats::{atomic::AggregatePeerStatsAtomic, snapshot::AggregatePeerStats};

use super::peer::{LivePeerState, Peer, PeerRx, PeerState, PeerTx};

pub mod stats;

#[derive(Default)]
pub(crate) struct PeerStates {
    pub stats: AggregatePeerStatsAtomic,
    pub states: DashMap<PeerHandle, Peer>,
}

impl PeerStates {
    pub fn stats(&self) -> AggregatePeerStats {
        AggregatePeerStats::from(&self.stats)
    }

    pub fn add_if_not_seen(&self, addr: SocketAddr) -> Option<PeerHandle> {
        use dashmap::mapref::entry::Entry;
        match self.states.entry(addr) {
            Entry::Occupied(_) => None,
            Entry::Vacant(vac) => {
                vac.insert(Default::default());
                atomic_inc(&self.stats.queued);
                atomic_inc(&self.stats.seen);
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
        self.with_peer(addr, |peer| peer.state.get_live().map(f))
            .flatten()
    }

    pub fn with_live_mut<R>(
        &self,
        addr: PeerHandle,
        reason: &'static str,
        f: impl FnOnce(&mut LivePeerState) -> R,
    ) -> Option<R> {
        self.with_peer_mut(addr, reason, |peer| peer.state.get_live_mut().map(f))
            .flatten()
    }

    pub fn drop_peer(&self, handle: PeerHandle) -> Option<Peer> {
        let p = self.states.remove(&handle).map(|r| r.1)?;
        self.stats.dec(p.state.get());
        Some(p)
    }

    pub fn mark_peer_interested(&self, handle: PeerHandle, is_interested: bool) -> Option<bool> {
        self.with_live_mut(handle, "mark_peer_interested", |live| {
            let prev = live.peer_interested;
            live.peer_interested = is_interested;
            prev
        })
    }
    pub fn update_bitfield_from_vec(&self, handle: PeerHandle, bitfield: Vec<u8>) -> Option<()> {
        self.with_live_mut(handle, "update_bitfield_from_vec", |live| {
            live.bitfield = BF::from_vec(bitfield);
        })
    }
    pub fn mark_peer_connecting(&self, h: PeerHandle) -> anyhow::Result<(PeerRx, PeerTx)> {
        let rx = self
            .with_peer_mut(h, "mark_peer_connecting", |peer| {
                peer.state
                    .queued_to_connecting(&self.stats)
                    .context("invalid peer state")
            })
            .context("peer not found in states")??;
        Ok(rx)
    }

    pub fn reset_peer_backoff(&self, handle: PeerHandle) {
        self.with_peer_mut(handle, "reset_peer_backoff", |p| {
            p.stats.backoff.reset();
        });
    }

    pub fn mark_peer_not_needed(&self, handle: PeerHandle) -> Option<PeerState> {
        let prev = self.with_peer_mut(handle, "mark_peer_not_needed", |peer| {
            peer.state.set_not_needed(&self.stats)
        })?;
        Some(prev)
    }
}
