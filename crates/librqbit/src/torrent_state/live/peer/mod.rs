pub mod stats;

use std::collections::HashSet;

use librqbit_core::hash_id::Id20;
use librqbit_core::lengths::{ChunkInfo, ValidPieceIndex};

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::peer_connection::WriterRequest;
use crate::type_aliases::BF;

use super::peers::stats::atomic::AggregatePeerStatsAtomic;

#[derive(Debug, Hash, PartialEq, Eq)]
pub(crate) struct InflightRequest {
    pub piece: ValidPieceIndex,
    pub chunk: u32,
}

impl From<&ChunkInfo> for InflightRequest {
    fn from(c: &ChunkInfo) -> Self {
        Self {
            piece: c.piece_index,
            chunk: c.chunk_index,
        }
    }
}

pub(crate) type PeerRx = UnboundedReceiver<WriterRequest>;
pub(crate) type PeerTx = UnboundedSender<WriterRequest>;

#[derive(Debug, Default)]
pub(crate) struct Peer {
    pub state: PeerStateNoMut,
    pub stats: stats::atomic::PeerStats,
}

impl Peer {
    pub fn new_live_for_incoming_connection(
        peer_id: Id20,
        tx: PeerTx,
        counters: &AggregatePeerStatsAtomic,
    ) -> Self {
        let state = PeerStateNoMut(PeerState::Live(LivePeerState::new(peer_id, tx)));
        counters.inc(&state.0);
        Self {
            state,
            stats: Default::default(),
        }
    }
}

#[derive(Debug, Default)]
pub(crate) enum PeerState {
    #[default]
    // Will be tried to be connected as soon as possible.
    Queued,
    Connecting(PeerTx),
    Live(LivePeerState),
    // There was an error, and it's waiting for exponential backoff.
    Dead,
    // We don't need to do anything with the peer any longer.
    // The peer has the full torrent, and we have the full torrent, so no need
    // to keep talking to it.
    NotNeeded,
}

impl std::fmt::Display for PeerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

impl PeerState {
    pub fn name(&self) -> &'static str {
        match self {
            PeerState::Queued => "queued",
            PeerState::Connecting(_) => "connecting",
            PeerState::Live(_) => "live",
            PeerState::Dead => "dead",
            PeerState::NotNeeded => "not needed",
        }
    }

    pub fn take_live_no_counters(self) -> Option<LivePeerState> {
        match self {
            PeerState::Live(l) => Some(l),
            _ => None,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct PeerStateNoMut(PeerState);

impl PeerStateNoMut {
    pub fn get(&self) -> &PeerState {
        &self.0
    }

    pub fn take(&mut self, counters: &AggregatePeerStatsAtomic) -> PeerState {
        self.set(Default::default(), counters)
    }

    pub fn set(&mut self, new: PeerState, counters: &AggregatePeerStatsAtomic) -> PeerState {
        counters.incdec(&self.0, &new);
        std::mem::replace(&mut self.0, new)
    }

    pub fn get_live(&self) -> Option<&LivePeerState> {
        match &self.0 {
            PeerState::Live(l) => Some(l),
            _ => None,
        }
    }

    pub fn get_live_mut(&mut self) -> Option<&mut LivePeerState> {
        match &mut self.0 {
            PeerState::Live(l) => Some(l),
            _ => None,
        }
    }

    pub fn queued_to_connecting(
        &mut self,
        counters: &AggregatePeerStatsAtomic,
    ) -> Option<(PeerRx, PeerTx)> {
        if let PeerState::Queued = &self.0 {
            let (tx, rx) = unbounded_channel();
            let tx_2 = tx.clone();
            self.set(PeerState::Connecting(tx), counters);
            Some((rx, tx_2))
        } else {
            None
        }
    }

    pub fn incoming_connection(
        &mut self,
        peer_id: Id20,
        tx: PeerTx,
        counters: &AggregatePeerStatsAtomic,
    ) -> anyhow::Result<()> {
        if matches!(&self.0, PeerState::Connecting(..) | PeerState::Live(..)) {
            anyhow::bail!("peer already active");
        }
        match self.take(counters) {
            PeerState::Queued | PeerState::Dead | PeerState::NotNeeded => {
                self.set(PeerState::Live(LivePeerState::new(peer_id, tx)), counters);
            }
            PeerState::Connecting(..) | PeerState::Live(..) => unreachable!(),
        }
        Ok(())
    }

    pub fn connecting_to_live(
        &mut self,
        peer_id: Id20,
        counters: &AggregatePeerStatsAtomic,
    ) -> Option<&mut LivePeerState> {
        if let PeerState::Connecting(_) = &self.0 {
            let tx = match self.take(counters) {
                PeerState::Connecting(tx) => tx,
                _ => unreachable!(),
            };
            self.set(PeerState::Live(LivePeerState::new(peer_id, tx)), counters);
            self.get_live_mut()
        } else {
            None
        }
    }

    pub fn set_not_needed(&mut self, counters: &AggregatePeerStatsAtomic) -> PeerState {
        self.set(PeerState::NotNeeded, counters)
    }
}

#[derive(Debug)]
pub(crate) struct LivePeerState {
    #[allow(dead_code)]
    peer_id: Id20,

    pub peer_interested: bool,

    // This is used to track the pieces the peer has.
    pub bitfield: BF,

    // When the peer sends us data this is used to track if we asked for it.
    pub inflight_requests: HashSet<InflightRequest>,

    // The main channel to send requests to peer.
    pub tx: PeerTx,
}

impl LivePeerState {
    pub fn new(peer_id: Id20, tx: PeerTx) -> Self {
        LivePeerState {
            peer_id,
            peer_interested: false,
            bitfield: BF::new(),
            inflight_requests: Default::default(),
            tx,
        }
    }

    pub fn has_full_torrent(&self, total_pieces: usize) -> bool {
        self.bitfield
            .get(0..total_pieces)
            .map_or(false, |s| s.all())
    }
}
