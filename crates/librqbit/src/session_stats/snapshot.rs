use serde::Serialize;

use crate::torrent_state::{peers::stats::snapshot::AggregatePeerStats, stats::Speed};

use super::SessionStats;

#[derive(Debug, Serialize)]
pub struct SessionStatsSnapshot {
    download_speed: Speed,
    upload_speed: Speed,
    peers: AggregatePeerStats,
}

impl From<&SessionStats> for SessionStatsSnapshot {
    fn from(s: &SessionStats) -> Self {
        Self {
            download_speed: s.down_speed_estimator.mbps().into(),
            upload_speed: s.up_speed_estimator.mbps().into(),
            peers: AggregatePeerStats::from(&s.atomic.peers),
        }
    }
}
