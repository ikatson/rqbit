use std::time::Duration;
use std::{collections::HashSet, sync::Arc};

use backoff::{ExponentialBackoff, ExponentialBackoffBuilder};
use librqbit_core::id20::Id20;
use librqbit_core::lengths::{ChunkInfo, ValidPieceIndex};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
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
pub type PeerTx = Arc<UnboundedSender<WriterRequest>>;

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
    pub state: PeerState,
    pub stats: PeerStats,
}

#[derive(Debug, Default)]
pub enum PeerState {
    #[default]
    Queued,
    Connecting(PeerTx),
    Live(LivePeerState),
    Dead,
}

#[derive(Debug)]
pub struct LivePeerState {
    pub peer_id: Id20,
    pub i_am_choked: bool,
    pub peer_interested: bool,
    pub requests_sem: Arc<Semaphore>,
    pub have_notify: Arc<Notify>,
    pub bitfield: Option<BF>,
    pub inflight_requests: HashSet<InflightRequest>,
    pub tx: PeerTx,
}

impl LivePeerState {
    pub fn new(peer_id: Id20, tx: PeerTx) -> Self {
        LivePeerState {
            peer_id,
            i_am_choked: true,
            peer_interested: false,
            bitfield: None,
            have_notify: Arc::new(Notify::new()),
            requests_sem: Arc::new(Semaphore::new(0)),
            inflight_requests: Default::default(),
            tx,
        }
    }
}
