use std::{collections::HashMap, sync::atomic::Ordering};

use serde::{Deserialize, Serialize};

use crate::torrent_state::live::peer::{Peer, PeerState};

#[derive(Serialize, Deserialize)]
pub struct PeerCounters {
    pub incoming_connections: u32,
    pub fetched_bytes: u64,
    pub total_time_connecting_ms: u64,
    pub connection_attempts: u32,
    pub connections: u32,
    pub errors: u32,
    pub fetched_chunks: u32,
    pub downloaded_and_checked_pieces: u32,
    pub total_piece_download_ms: u64,
}

#[derive(Serialize, Deserialize)]
pub struct PeerStats {
    pub counters: PeerCounters,
    pub state: &'static str,
}

impl From<&super::atomic::PeerCountersAtomic> for PeerCounters {
    fn from(counters: &super::atomic::PeerCountersAtomic) -> Self {
        Self {
            incoming_connections: counters.incoming_connections.load(Ordering::Relaxed),
            fetched_bytes: counters.fetched_bytes.load(Ordering::Relaxed),
            total_time_connecting_ms: counters.total_time_connecting_ms.load(Ordering::Relaxed),
            connection_attempts: counters
                .outgoing_connection_attempts
                .load(Ordering::Relaxed),
            connections: counters.outgoing_connections.load(Ordering::Relaxed),
            errors: counters.errors.load(Ordering::Relaxed),
            fetched_chunks: counters.fetched_chunks.load(Ordering::Relaxed),
            downloaded_and_checked_pieces: counters
                .downloaded_and_checked_pieces
                .load(Ordering::Relaxed),
            total_piece_download_ms: counters.total_piece_download_ms.load(Ordering::Relaxed),
        }
    }
}

impl From<&Peer> for PeerStats {
    fn from(peer: &Peer) -> Self {
        Self {
            counters: peer.stats.counters.as_ref().into(),
            state: peer.state.get().name(),
        }
    }
}

#[derive(Serialize)]
pub struct PeerStatsSnapshot {
    pub peers: HashMap<String, PeerStats>,
}

#[derive(Clone, Copy, Default, Deserialize)]
pub enum PeerStatsFilterState {
    All,
    #[default]
    Live,
}

impl PeerStatsFilterState {
    pub(crate) fn matches(&self, s: &PeerState) -> bool {
        matches!((self, s), (Self::All, _) | (Self::Live, PeerState::Live(_)))
    }
}

#[derive(Default, Deserialize)]
pub struct PeerStatsFilter {
    pub state: PeerStatsFilterState,
}
