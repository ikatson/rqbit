use std::sync::atomic::Ordering;

use serde::Serialize;

use crate::torrent_state::{peers::stats::snapshot::AggregatePeerStats, stats::Speed};

use super::SessionStats;

#[derive(Debug, Serialize)]
pub struct SessionStatsSnapshot {
    fetched_bytes: u64,
    uploaded_bytes: u64,
    download_speed: Speed,
    upload_speed: Speed,
    peers: AggregatePeerStats,
    uptime_seconds: u64,
}

impl From<&SessionStats> for SessionStatsSnapshot {
    fn from(s: &SessionStats) -> Self {
        Self {
            download_speed: s.down_speed_estimator.mbps().into(),
            upload_speed: s.up_speed_estimator.mbps().into(),
            fetched_bytes: s.atomic.fetched_bytes.load(Ordering::Relaxed),
            uploaded_bytes: s.atomic.uploaded_bytes.load(Ordering::Relaxed),
            peers: AggregatePeerStats::from(&s.atomic.peers),
            uptime_seconds: s.startup_time.elapsed().as_secs(),
        }
    }
}
