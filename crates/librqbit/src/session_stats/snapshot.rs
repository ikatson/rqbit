use std::sync::atomic::Ordering;

use serde::Serialize;

use crate::torrent_state::{peers::stats::snapshot::AggregatePeerStats, stats::Speed};

use super::SessionStats;

#[derive(Debug, Serialize)]
pub struct SessionStatsSnapshot {
    pub fetched_bytes: u64,
    pub uploaded_bytes: u64,
    pub blocked_incoming: u64,
    pub blocked_outgoing: u64,
    pub download_speed: Speed,
    pub upload_speed: Speed,
    pub peers: AggregatePeerStats,
    pub uptime_seconds: u64,
}

impl From<&SessionStats> for SessionStatsSnapshot {
    fn from(s: &SessionStats) -> Self {
        Self {
            download_speed: s.down_speed_estimator.mbps().into(),
            upload_speed: s.up_speed_estimator.mbps().into(),
            fetched_bytes: s.atomic.fetched_bytes.load(Ordering::Relaxed),
            uploaded_bytes: s.atomic.uploaded_bytes.load(Ordering::Relaxed),
            blocked_incoming: s.atomic.blocked_incoming.load(Ordering::Relaxed),
            blocked_outgoing: s.atomic.blocked_incoming.load(Ordering::Relaxed),
            peers: AggregatePeerStats::from(&s.atomic.peers),
            uptime_seconds: s.startup_time.elapsed().as_secs(),
        }
    }
}

impl SessionStatsSnapshot {
    pub fn as_prometheus(&self, mut out: &mut String) {
        use core::fmt::Write;

        out.push('\n');

        macro_rules! m {
            ($type:ident, $name:ident, $value:expr) => {{
                writeln!(
                    &mut out,
                    concat!("# TYPE ", stringify!($name), " ", stringify!($type))
                )
                .unwrap();
                writeln!(&mut out, concat!(stringify!($name), " {}"), $value).unwrap();
            }};
        }

        m!(counter, rqbit_fetched_bytes, self.fetched_bytes);
        m!(counter, rqbit_uploaded_bytes, self.uploaded_bytes);
        m!(
            gauge,
            rqbit_download_speed_bytes,
            self.download_speed.as_bytes()
        );
        m!(
            gauge,
            rqbit_upload_speed_bytes,
            self.upload_speed.as_bytes()
        );
        m!(gauge, rqbit_uptime_seconds, self.uptime_seconds);
        m!(gauge, rqbit_peers_connecting, self.peers.connecting);
        writeln!(&mut out, "# TYPE rqbit_peers_live gauge").unwrap();
        writeln!(
            &mut out,
            "rqbit_peers_live{{kind=\"tcp\"}} {}",
            self.peers.live_tcp
        )
        .unwrap();
        writeln!(
            &mut out,
            "rqbit_peers_live{{kind=\"utp\"}} {}",
            self.peers.live_utp
        )
        .unwrap();
        writeln!(
            &mut out,
            "rqbit_peers_live{{kind=\"socks\"}} {}",
            self.peers.live_socks
        )
        .unwrap();
        m!(gauge, rqbit_peers_dead, self.peers.dead);
        m!(gauge, rqbit_peers_not_needed, self.peers.not_needed);
        m!(gauge, rqbit_peers_queued, self.peers.queued);
        m!(gauge, rqbit_peers_queued, self.peers.seen);
        m!(gauge, rqbit_peers_steals, self.peers.steals);
    }
}
