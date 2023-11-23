use std::{collections::HashMap, sync::atomic::Ordering};

use serde::{Deserialize, Serialize};

use crate::torrent_state::live::peer::{Peer, PeerState};

#[derive(Serialize, Deserialize)]
pub struct PeerCounters {
    pub fetched_bytes: u64,
    pub total_time_connecting_ms: u64,
    pub connection_attempts: u32,
    pub connections: u32,
    pub errors: u32,
    pub fetched_chunks: u32,
    pub downloaded_and_checked_pieces: u32,
}

#[derive(Serialize, Deserialize)]
pub struct PeerStats {
    pub counters: PeerCounters,
    pub state: &'static str,
}

impl From<&super::atomic::PeerCounters> for PeerCounters {
    fn from(counters: &super::atomic::PeerCounters) -> Self {
        Self {
            fetched_bytes: counters.fetched_bytes.load(Ordering::Relaxed),
            total_time_connecting_ms: counters.total_time_connecting_ms.load(Ordering::Relaxed),
            connection_attempts: counters.connection_attempts.load(Ordering::Relaxed),
            connections: counters.connections.load(Ordering::Relaxed),
            errors: counters.errors.load(Ordering::Relaxed),
            fetched_chunks: counters.fetched_chunks.load(Ordering::Relaxed),
            downloaded_and_checked_pieces: counters
                .downloaded_and_checked_pieces
                .load(Ordering::Relaxed),
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
    pub fn matches(&self, s: &PeerState) -> bool {
        match (self, s) {
            (Self::All, _) => true,
            (Self::Live, PeerState::Live(_)) => true,
            _ => false,
        }
    }
}

#[derive(Default, Deserialize)]
pub struct PeerStatsFilter {
    pub state: PeerStatsFilterState,
}
