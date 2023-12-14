use std::time::Duration;

use serde::Serialize;

use crate::torrent_state::live::peers::stats::snapshot::AggregatePeerStats;

#[derive(Debug, Serialize, Default)]
pub struct StatsSnapshot {
    pub downloaded_and_checked_bytes: u64,

    pub fetched_bytes: u64,
    pub uploaded_bytes: u64,

    pub downloaded_and_checked_pieces: u64,
    pub total_piece_download_ms: u64,
    pub peer_stats: AggregatePeerStats,
}

impl StatsSnapshot {
    pub fn average_piece_download_time(&self) -> Option<Duration> {
        let d = self.downloaded_and_checked_pieces;
        let t = self.total_piece_download_ms;
        if d == 0 {
            return None;
        }
        Some(Duration::from_secs_f64(t as f64 / d as f64 / 1000f64))
    }
}
