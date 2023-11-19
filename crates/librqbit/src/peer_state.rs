use std::time::Duration;
use std::{collections::HashSet, sync::Arc};

use anyhow::Context;
use backoff::{ExponentialBackoff, ExponentialBackoffBuilder};
use librqbit_core::id20::Id20;
use librqbit_core::lengths::{ChunkInfo, ValidPieceIndex};
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
    pub state: PeerState,
    pub stats: PeerStats,
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

    fn take_connecting(&mut self) -> Option<PeerTx> {
        if let PeerState::Connecting(_) = self {
            match std::mem::take(self) {
                PeerState::Connecting(tx) => Some(tx),
                _ => unreachable!(),
            }
        } else {
            None
        }
    }

    pub fn take_live(&mut self) -> Option<LivePeerState> {
        if let PeerState::Live(_) = self {
            match std::mem::take(self) {
                PeerState::Live(l) => Some(l),
                _ => unreachable!(),
            }
        } else {
            None
        }
    }

    pub fn get_live_mut(&mut self) -> Option<&mut LivePeerState> {
        match self {
            PeerState::Live(l) => Some(l),
            _ => None,
        }
    }

    pub fn queued_to_connecting(&mut self) -> Option<PeerRx> {
        if let PeerState::Queued = self {
            let (tx, rx) = unbounded_channel();
            *self = PeerState::Connecting(tx);
            Some(rx)
        } else {
            None
        }
    }
    pub fn connecting_to_live(&mut self, peer_id: Id20) -> Option<&mut LivePeerState> {
        let tx = self.take_connecting()?;
        *self = PeerState::Live(LivePeerState::new(peer_id, tx));
        self.get_live_mut()
    }

    pub fn dead_to_queued(&mut self) -> bool {
        if let PeerState::Dead = self {
            *self = PeerState::Queued;
            return true;
        }
        false
    }

    pub fn to_dead(&mut self) -> Option<Option<LivePeerState>> {
        match std::mem::replace(self, PeerState::Dead) {
            PeerState::Live(l) => Some(Some(l)),
            PeerState::Connecting(_) => Some(None),
            _ => None,
        }
    }

    pub fn to_not_needed(&mut self) -> Option<LivePeerState> {
        match std::mem::replace(self, PeerState::NotNeeded) {
            PeerState::Live(l) => Some(l),
            _ => None,
        }
    }
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

    pub fn has_full_torrent(&self, total_pieces: usize) -> bool {
        let bf = match self.bitfield.as_ref() {
            Some(bf) => bf,
            None => return false,
        };
        bf.get(0..total_pieces).map_or(false, |s| s.all())
    }
}
