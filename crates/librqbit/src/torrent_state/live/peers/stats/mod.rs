use portable_atomic::AtomicU32;

use crate::{
    stream_connect::ConnectionKind,
    torrent_state::{
        live::peer::PeerState,
        peer::LivePeerState,
        utils::{atomic_dec, atomic_inc},
    },
};

gen_stats!(AggregatePeerStatsAtomic AggregatePeerStats, [
    queued u32,
    connecting u32,
    live u32,
    live_tcp u32,
    live_utp u32,
    live_socks u32,
    seen u32,
    dead u32,
    not_needed u32,
    steals u32
], []);

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

    pub(crate) fn inc(&self, state: &PeerState) {
        if let PeerState::Live(l) = state {
            atomic_inc(self.live_kind_counter(l));
        }
        atomic_inc(self.counter(state));
    }

    pub(crate) fn dec(&self, state: &PeerState) {
        if let PeerState::Live(l) = state {
            atomic_dec(self.live_kind_counter(l));
        }
        atomic_dec(self.counter(state));
    }

    pub(crate) fn incdec(&self, old: &PeerState, new: &PeerState) {
        self.dec(old);
        self.inc(new);
    }

    pub fn inc_steals(&self) {
        atomic_inc(&self.steals);
    }
}
