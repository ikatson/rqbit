use std::sync::atomic::AtomicU32;

use serde::Serialize;

use crate::torrent_state::{
    live::peer::PeerState,
    utils::{atomic_dec, atomic_inc},
};

#[derive(Debug, Default, Serialize)]
pub(crate) struct AggregatePeerStatsAtomic {
    pub queued: AtomicU32,
    pub connecting: AtomicU32,
    pub live: AtomicU32,
    pub seen: AtomicU32,
    pub dead: AtomicU32,
    pub not_needed: AtomicU32,
}

impl AggregatePeerStatsAtomic {
    pub fn counter(&self, state: &PeerState) -> &AtomicU32 {
        match state {
            PeerState::Connecting(_) => &self.connecting,
            PeerState::Live(_) => &self.live,
            PeerState::Queued => &self.queued,
            PeerState::Dead => &self.dead,
            PeerState::NotNeeded => &self.not_needed,
        }
    }

    pub fn inc(&self, state: &PeerState) {
        atomic_inc(self.counter(state));
    }

    pub fn dec(&self, state: &PeerState) {
        atomic_dec(self.counter(state));
    }

    pub fn incdec(&self, old: &PeerState, new: &PeerState) {
        self.dec(old);
        self.inc(new);
    }
}
