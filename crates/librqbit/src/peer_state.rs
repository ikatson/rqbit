use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use std::{collections::HashSet, sync::Arc};

use anyhow::Context;
use backoff::{ExponentialBackoff, ExponentialBackoffBuilder};
use librqbit_core::id20::Id20;
use librqbit_core::lengths::{ChunkInfo, ValidPieceIndex};
use serde::Serialize;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::{Notify, Semaphore};

use crate::peer_connection::WriterRequest;
use crate::type_aliases::BF;

#[derive(Debug, Hash, PartialEq, Eq)]
pub struct InflightRequest {
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

// TODO: Arc can be removed probably, as UnboundedSender should be clone + it can be downgraded to weak.
pub type PeerRx = UnboundedReceiver<WriterRequest>;
pub type PeerTx = UnboundedSender<WriterRequest>;

pub trait SendMany {
    fn send_many(&self, requests: impl IntoIterator<Item = WriterRequest>) -> anyhow::Result<()>;
}

impl SendMany for PeerTx {
    fn send_many(&self, requests: impl IntoIterator<Item = WriterRequest>) -> anyhow::Result<()> {
        requests
            .into_iter()
            .try_for_each(|r| self.send(r))
            .context("peer dropped")
    }
}

#[derive(Debug)]
pub struct PeerStats {
    pub backoff: ExponentialBackoff,
}

impl Default for PeerStats {
    fn default() -> Self {
        Self {
            backoff: ExponentialBackoffBuilder::new()
                .with_initial_interval(Duration::from_secs(10))
                .with_multiplier(6.)
                .with_max_interval(Duration::from_secs(3600))
                .with_max_elapsed_time(Some(Duration::from_secs(86400)))
                .build(),
        }
    }
}

#[derive(Debug, Default)]
pub struct Peer {
    pub state: PeerStateNoMut,
    pub stats: PeerStats,
}

#[derive(Debug, Default, Serialize)]
pub struct AggregatePeerStatsAtomic {
    pub queued: AtomicU32,
    pub connecting: AtomicU32,
    pub live: AtomicU32,
    pub seen: AtomicU32,
    pub dead: AtomicU32,
    pub not_needed: AtomicU32,
}

pub fn atomic_inc(c: &AtomicU32) -> u32 {
    c.fetch_add(1, Ordering::Relaxed)
}

pub fn atomic_dec(c: &AtomicU32) -> u32 {
    c.fetch_sub(1, Ordering::Relaxed)
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

#[derive(Debug, Default)]
pub enum PeerState {
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
pub struct PeerStateNoMut(PeerState);

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

    pub fn queued_to_connecting(&mut self, counters: &AggregatePeerStatsAtomic) -> Option<PeerRx> {
        if let PeerState::Queued = &self.0 {
            let (tx, rx) = unbounded_channel();
            self.set(PeerState::Connecting(tx), counters);
            Some(rx)
        } else {
            None
        }
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

    pub fn to_dead(&mut self, counters: &AggregatePeerStatsAtomic) -> PeerState {
        self.set(PeerState::Dead, counters)
    }

    pub fn to_not_needed(&mut self, counters: &AggregatePeerStatsAtomic) -> PeerState {
        self.set(PeerState::NotNeeded, counters)
    }
}

#[derive(Debug)]
pub struct LivePeerState {
    pub peer_id: Id20,
    pub i_am_choked: bool,
    pub peer_interested: bool,

    // This is used to limit the number of chunk requests we send to a peer at a time.
    pub requests_sem: Arc<Semaphore>,

    // This is used to track the pieces the peer has.
    pub bitfield: BF,

    // This is used to only request a piece from a peer once when stealing from others.
    // So that you don't steal then re-steal the same piece in a loop.
    pub previously_requested_pieces: BF,

    // When the peer sends us data this is used to track if we asked for it.
    pub inflight_requests: HashSet<InflightRequest>,

    // The main channel to send requests to peer.
    pub tx: PeerTx,
}

impl LivePeerState {
    pub fn new(peer_id: Id20, tx: PeerTx) -> Self {
        LivePeerState {
            peer_id,
            i_am_choked: true,
            peer_interested: false,
            bitfield: BF::new(),
            previously_requested_pieces: BF::new(),
            requests_sem: Arc::new(Semaphore::new(0)),
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
