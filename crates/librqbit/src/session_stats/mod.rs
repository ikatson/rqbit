use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use anyhow::Context;
use librqbit_core::speed_estimator::{SpeedEstimator, Updater};
use snapshot::SessionStatsSnapshot;
use tracing::debug_span;

use crate::{Session, torrent_state::peers::stats::AggregatePeerStatsAtomic};

pub mod snapshot;

gen_stats!(SessionCountersAtomic SessionCountersSnapshot, [
    fetched_bytes u64,
    uploaded_bytes u64,
    blocked_incoming u64,
    blocked_outgoing u64
], []);

pub struct SessionStats {
    pub counters: SessionCountersAtomic,
    pub peers: Arc<AggregatePeerStatsAtomic>,
    pub down_speed_estimator: SpeedEstimator,
    pub up_speed_estimator: SpeedEstimator,
    pub startup_time: Instant,
}

impl SessionStats {
    pub fn new() -> (Self, Updater, Updater) {
        let (down, down_upd) = SpeedEstimator::new(5);
        let (up, up_upd) = SpeedEstimator::new(5);
        (
            SessionStats {
                counters: Default::default(),
                peers: Default::default(),
                down_speed_estimator: down,
                up_speed_estimator: up,
                startup_time: Instant::now(),
            },
            down_upd,
            up_upd,
        )
    }
}

impl Session {
    pub(crate) fn start_speed_estimator_updater(
        self: &Arc<Self>,
        mut down_upd: Updater,
        mut up_upd: Updater,
    ) {
        self.spawn(
            debug_span!(parent: self.rs(), "speed_estimator"),
            "speed_estimator",
            {
                let s = Arc::downgrade(self);

                async move {
                    let mut i = tokio::time::interval(Duration::from_secs(1));
                    loop {
                        i.tick().await;
                        let s = s.upgrade().context("session is dead")?;
                        let now = Instant::now();
                        let fetched = s.stats.counters.fetched_bytes.load(Ordering::Relaxed);
                        let uploaded = s.stats.counters.uploaded_bytes.load(Ordering::Relaxed);
                        down_upd.add_snapshot(fetched, None, now);
                        up_upd.add_snapshot(uploaded, None, now);
                    }
                }
            },
        )
    }

    pub fn stats_snapshot(&self) -> SessionStatsSnapshot {
        SessionStatsSnapshot::from((&*self.stats, self.connector.stats().snapshot()))
    }
}
