use std::{
    sync::{
        atomic::{AtomicU32, AtomicU64},
        Arc,
    },
    time::Duration,
};

use backoff::{ExponentialBackoff, ExponentialBackoffBuilder};

#[derive(Default, Debug)]
pub struct PeerCounters {
    pub fetched_bytes: AtomicU64,
    pub total_time_connecting_ms: AtomicU64,
    pub connection_attempts: AtomicU32,
    pub connections: AtomicU32,
    pub errors: AtomicU32,
    pub fetched_chunks: AtomicU32,
    pub downloaded_and_checked_pieces: AtomicU32,
    pub downloaded_and_checked_bytes: AtomicU64,
}

#[derive(Debug)]
pub struct PeerStats {
    pub counters: Arc<PeerCounters>,
    pub backoff: ExponentialBackoff,
}

impl Default for PeerStats {
    fn default() -> Self {
        Self {
            counters: Arc::new(Default::default()),
            backoff: ExponentialBackoffBuilder::new()
                .with_initial_interval(Duration::from_secs(10))
                .with_multiplier(6.)
                .with_max_interval(Duration::from_secs(3600))
                .with_max_elapsed_time(Some(Duration::from_secs(86400)))
                .build(),
        }
    }
}
