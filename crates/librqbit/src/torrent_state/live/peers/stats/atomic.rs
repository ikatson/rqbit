use std::sync::atomic::AtomicU32;

use serde::Serialize;

use crate::{
    stream_connect::ConnectionKind,
    torrent_state::{
        live::peer::PeerState,
        peer::LivePeerState,
        utils::{atomic_dec, atomic_inc},
    },
};

#[derive(Debug, Default, Serialize)]
pub(crate) struct AggregatePeerStatsAtomic {
    pub queued: AtomicU32,
    pub connecting: AtomicU32,
    pub live: AtomicU32,
    pub live_tcp: AtomicU32,
    pub live_utp: AtomicU32,
    pub live_socks: AtomicU32,
    pub seen: AtomicU32,
    pub dead: AtomicU32,
    pub not_needed: AtomicU32,
    pub steals: AtomicU32,
}

impl AggregatePeerStatsAtomic {
    fn counter(&self, state: &PeerState) -> &AtomicU32 {
        match state {
            PeerState::Connecting(_) => &self.connecting,
            PeerState::Live(_) => &self.live,
            PeerState::Queued => &self.queued,
            PeerState::Dead => &self.dead,
            PeerState::NotNeeded => &self.not_needed,
        }
    }

    fn live_kind_counter(&self, l: &LivePeerState) -> &AtomicU32 {
        match l.connection_kind {
            ConnectionKind::Tcp => &self.live_tcp,
            ConnectionKind::Utp => &self.live_utp,
            ConnectionKind::Socks => &self.live_socks,
        }
    }

    pub fn inc(&self, state: &PeerState) {
        if let PeerState::Live(l) = state {
            atomic_inc(self.live_kind_counter(l));
        }
        atomic_inc(self.counter(state));
    }

    pub fn dec(&self, state: &PeerState) {
        if let PeerState::Live(l) = state {
            atomic_dec(self.live_kind_counter(l));
        }
        atomic_dec(self.counter(state));
    }

    pub fn incdec(&self, old: &PeerState, new: &PeerState) {
        self.dec(old);
        self.inc(new);
    }

    pub fn inc_steals(&self) {
        atomic_inc(&self.steals);
    }
}
