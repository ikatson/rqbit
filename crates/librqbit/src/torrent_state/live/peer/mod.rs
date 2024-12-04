pub mod stats;

use std::collections::HashSet;
use std::net::SocketAddr;

use librqbit_core::hash_id::Id20;
use librqbit_core::lengths::ChunkInfo;

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::debug;

use crate::peer_connection::WriterRequest;
use crate::type_aliases::BF;

use super::PeerStates;

pub(crate) type InflightRequest = ChunkInfo;
pub(crate) type PeerRx = UnboundedReceiver<WriterRequest>;
pub(crate) type PeerTx = UnboundedSender<WriterRequest>;

#[derive(Debug, Default)]
pub(crate) struct Peer {
    pub state: PeerStateNoMut,
    pub stats: stats::atomic::PeerStats,
    pub outgoing_address: Option<SocketAddr>,
}

impl Peer {
    pub fn new_live_for_incoming_connection(
        peer_id: Id20,
        tx: PeerTx,
        counters: &PeerStates,
    ) -> Self {
        let state = PeerStateNoMut(PeerState::Live(LivePeerState::new(peer_id, tx, true)));
        for counter in [&counters.session_stats.peers, &counters.stats] {
            counter.inc(&state.0);
        }
        Self {
            state,
            stats: Default::default(),
            outgoing_address: None,
        }
    }

    pub fn new_with_outgoing_address(addr: SocketAddr) -> Self {
        Self {
            outgoing_address: Some(addr),
            ..Default::default()
        }
    }

    pub(crate) fn reconnect_not_needed_peer(
        &mut self,
        known_address: SocketAddr,
        counters: &PeerStates,
    ) -> Option<SocketAddr> {
        if let PeerState::NotNeeded = self.state.get() {
            match self.outgoing_address {
                None => None,
                Some(socket_addr) => {
                    if known_address == socket_addr {
                        self.state.set(PeerState::Queued, counters);
                    } else {
                        debug!(
                            peer = known_address.to_string(),
                            outgoing_addr = socket_addr.to_string(),
                            "peer will by retried on different address",
                        );
                    }
                    Some(socket_addr)
                }
            }
        } else {
            None
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

    pub fn take(&mut self, counters: &PeerStates) -> PeerState {
        self.set(Default::default(), counters)
    }

    pub fn destroy(self, counters: &PeerStates) {
        for counter in [&counters.session_stats.peers, &counters.stats] {
            counter.dec(&self.0);
        }
    }

    pub fn set(&mut self, new: PeerState, counters: &PeerStates) -> PeerState {
        for counter in [&counters.session_stats.peers, &counters.stats] {
            counter.incdec(&self.0, &new);
        }
        std::mem::replace(&mut self.0, new)
    }

    pub fn get_live(&self) -> Option<&LivePeerState> {
        match &self.0 {
            PeerState::Live(l) => Some(l),
            _ => None,
        }
    }

    pub fn is_live(&self) -> bool {
        matches!(&self.0, PeerState::Live(_))
    }

    pub fn get_live_mut(&mut self) -> Option<&mut LivePeerState> {
        match &mut self.0 {
            PeerState::Live(l) => Some(l),
            _ => None,
        }
    }

    pub fn idle_to_connecting(&mut self, counters: &PeerStates) -> Option<(PeerRx, PeerTx)> {
        match &self.0 {
            PeerState::Queued | PeerState::NotNeeded => {
                let (tx, rx) = unbounded_channel();
                let tx_2 = tx.clone();
                self.set(PeerState::Connecting(tx), counters);
                Some((rx, tx_2))
            }
            _ => None,
        }
    }

    pub fn incoming_connection(
        &mut self,
        peer_id: Id20,
        tx: PeerTx,
        counters: &PeerStates,
    ) -> anyhow::Result<()> {
        if matches!(&self.0, PeerState::Connecting(..) | PeerState::Live(..)) {
            anyhow::bail!("peer already active");
        }
        match self.take(counters) {
            PeerState::Queued | PeerState::Dead | PeerState::NotNeeded => {
                self.set(
                    PeerState::Live(LivePeerState::new(peer_id, tx, true)),
                    counters,
                );
            }
            PeerState::Connecting(..) | PeerState::Live(..) => unreachable!(),
        }
        Ok(())
    }

    pub fn connecting_to_live(
        &mut self,
        peer_id: Id20,
        counters: &PeerStates,
    ) -> Option<&mut LivePeerState> {
        if let PeerState::Connecting(_) = &self.0 {
            let tx = match self.take(counters) {
                PeerState::Connecting(tx) => tx,
                _ => unreachable!(),
            };
            self.set(
                PeerState::Live(LivePeerState::new(peer_id, tx, false)),
                counters,
            );
            self.get_live_mut()
        } else {
            None
        }
    }

    pub fn set_not_needed(&mut self, counters: &PeerStates) -> PeerState {
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
    pub fn new(peer_id: Id20, tx: PeerTx, initial_interested: bool) -> Self {
        LivePeerState {
            peer_id,
            peer_interested: initial_interested,
            bitfield: BF::default(),
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
