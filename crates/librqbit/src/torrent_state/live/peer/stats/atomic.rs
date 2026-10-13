use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

use backon::{BackoffBuilder, ExponentialBackoff, ExponentialBuilder};

#[derive(Default, Debug)]
pub(crate) struct PeerCountersAtomic {
    pub fetched_bytes: AtomicU64,
    pub uploaded_bytes: AtomicU64,
    pub total_time_connecting_ms: AtomicU64,
    pub incoming_connections: AtomicU32,
    pub outgoing_connection_attempts: AtomicU32,
    pub outgoing_connections: AtomicU32,
    pub errors: AtomicU32,
    pub fetched_chunks: AtomicU32,
    pub downloaded_and_checked_pieces: AtomicU32,
    pub downloaded_and_checked_bytes: AtomicU64,
    pub total_piece_download_ms: AtomicU64,
    pub times_stolen_from_me: AtomicU32,
    pub times_i_stole: AtomicU32,
}

impl PeerCountersAtomic {
    pub(crate) fn on_piece_completed(&self, piece_len: u64, elapsed: Duration) {
        #[allow(clippy::cast_possible_truncation)]
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

// Reconnect backoff for an outgoing peer that died with an error.
//
// A single transient failure (connect refused, a seeder briefly at its
// connection limit, a reset) used to cost 10-20 s (min 10 s, factor 6, plus
// jitter) before the first retry, and the backoff was only reset by a fully
// downloaded piece. For streaming that is the difference between a 1 s and a
// 15 s time-to-first-byte. Retry quickly first (1-2 s) and back off
// gradually; dead peers still end up at one attempt every 5-10 minutes.
// The backoff is also reset as soon as a connection delivers data (see
// `PeerHandler::on_received_piece`), not only on a completed piece.
pub(crate) const BACKOFF_MIN_DELAY: Duration = Duration::from_secs(1);
pub(crate) const BACKOFF_FACTOR: f32 = 2.;
pub(crate) const BACKOFF_MAX_DELAY: Duration = Duration::from_secs(300);

fn backoff() -> ExponentialBackoff {
    ExponentialBuilder::new()
        .with_min_delay(BACKOFF_MIN_DELAY)
        .with_factor(BACKOFF_FACTOR)
        .with_jitter()
        .with_max_delay(BACKOFF_MAX_DELAY)
        .with_total_delay(Some(Duration::from_secs(86400)))
        .without_max_times()
        .build()
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
            backoff: backoff(),
        }
    }
}

impl PeerStats {
    pub fn reset_backoff(&mut self) {
        self.backoff = backoff();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{BACKOFF_MAX_DELAY, PeerStats};

    #[test]
    fn test_backoff_first_retry_is_short_and_reset_restarts_it() {
        let mut stats = PeerStats::default();
        // Jitter adds up to 100% of the current delay.
        let first = stats.backoff.next().unwrap();
        assert!(
            first >= Duration::from_secs(1) && first < Duration::from_secs(2),
            "{first:?}"
        );
        let second = stats.backoff.next().unwrap();
        assert!(
            second >= Duration::from_secs(2) && second < Duration::from_secs(4),
            "{second:?}"
        );
        // Grows, but is capped (max delay + jitter).
        let mut last = second;
        for _ in 0..20 {
            last = stats.backoff.next().unwrap();
        }
        assert!(
            last >= BACKOFF_MAX_DELAY && last < BACKOFF_MAX_DELAY * 2,
            "{last:?}"
        );
        stats.reset_backoff();
        let again = stats.backoff.next().unwrap();
        assert!(again < Duration::from_secs(2), "{again:?}");
    }
}
