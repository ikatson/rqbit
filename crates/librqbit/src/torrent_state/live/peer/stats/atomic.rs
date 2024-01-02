use std::{
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use backoff::{ExponentialBackoff, ExponentialBackoffBuilder};

#[derive(Default, Debug)]
pub(crate) struct PeerCountersAtomic {
    pub fetched_bytes: AtomicU64,
    pub total_time_connecting_ms: AtomicU64,
    pub incoming_connections: AtomicU32,
    pub outgoing_connection_attempts: AtomicU32,
    pub outgoing_connections: AtomicU32,
    pub errors: AtomicU32,
    pub fetched_chunks: AtomicU32,
    pub downloaded_and_checked_pieces: AtomicU32,
    pub downloaded_and_checked_bytes: AtomicU64,
    pub total_piece_download_ms: AtomicU64,
}

impl PeerCountersAtomic {
    pub(crate) fn on_piece_downloaded(&self, piece_len: u64, elapsed: Duration) {
        let elapsed = elapsed.as_millis() as u64;
        self.total_piece_download_ms
            .fetch_add(elapsed, Ordering::Release);
        self.downloaded_and_checked_pieces
            .fetch_add(1, Ordering::Release);
        self.downloaded_and_checked_bytes
            .fetch_add(piece_len, Ordering::Relaxed);
    }

    pub(crate) fn average_piece_download_time(&self) -> Option<Duration> {
        let downloaded_pieces = self.downloaded_and_checked_pieces.load(Ordering::Acquire);
        let total_download_time = self.total_piece_download_ms.load(Ordering::Acquire);
        if total_download_time == 0 || downloaded_pieces == 0 {
            return None;
        }
        Some(Duration::from_millis(
            total_download_time / downloaded_pieces as u64,
        ))
    }
}

#[derive(Debug)]
pub(crate) struct PeerStats {
    pub counters: Arc<PeerCountersAtomic>,
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
