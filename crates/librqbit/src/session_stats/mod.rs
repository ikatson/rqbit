use std::{
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

use atomic::AtomicSessionStats;
use librqbit_core::speed_estimator::SpeedEstimator;
use tracing::error_span;

use crate::Session;

pub mod atomic;

pub struct SessionStats {
    pub atomic: Arc<AtomicSessionStats>,
    pub down_speed_estimator: SpeedEstimator,
    pub up_speed_estimator: SpeedEstimator,
}

impl SessionStats {
    pub fn new() -> Self {
        SessionStats {
            atomic: Default::default(),
            down_speed_estimator: SpeedEstimator::new(5),
            up_speed_estimator: SpeedEstimator::new(5),
        }
    }
}

impl Session {
    pub(crate) fn start_speed_estimator_updater(self: &Arc<Self>) {
        self.spawn(error_span!(parent: self.rs(), "speed_estimator"), {
            let s = self.clone();

            async move {
                let mut i = tokio::time::interval(Duration::from_secs(1));
                loop {
                    i.tick().await;
                    let now = Instant::now();
                    let fetched = s.stats.atomic.fetched_bytes.load(Ordering::Relaxed);
                    let uploaded = s.stats.atomic.uploaded_bytes.load(Ordering::Relaxed);
                    s.stats
                        .down_speed_estimator
                        .add_snapshot(fetched, None, now);
                    s.stats.up_speed_estimator.add_snapshot(uploaded, None, now);
                }
            }
        })
    }
}
