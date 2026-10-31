use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

use backon::{BackoffBuilder, ExponentialBackoff, ExponentialBuilder};

use crate::PeerBackoffConfig;

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

fn backoff(config: &PeerBackoffConfig) -> ExponentialBackoff {
    ExponentialBuilder::new()
        .with_min_delay(config.min_delay)
        .with_factor(config.factor)
        .with_jitter()
        .with_max_delay(config.max_delay)
        .with_total_delay(config.total_delay)
        .without_max_times()
        .build()
}

#[derive(Debug)]
pub(crate) struct PeerStats {
    pub counters: Arc<PeerCountersAtomic>,
    pub backoff: ExponentialBackoff,
    backoff_config: PeerBackoffConfig,
}

impl Default for PeerStats {
    fn default() -> Self {
        Self::new(PeerBackoffConfig::default())
    }
}

impl PeerStats {
    pub fn new(backoff_config: PeerBackoffConfig) -> Self {
        Self {
            counters: Arc::new(Default::default()),
            backoff: backoff(&backoff_config),
            backoff_config,
        }
    }

    pub fn reset_backoff(&mut self) {
        self.backoff = backoff(&self.backoff_config);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    // The jitter adds up to 100% of the delay.
    fn assert_in(d: Option<Duration>, min: Duration) {
        let d = d.expect("backoff ended");
        assert!(
            d >= min && d <= min * 2,
            "{d:?} not in [{min:?}, {:?}]",
            min * 2
        );
    }

    #[test]
    fn default_config_is_the_historic_schedule() {
        let mut stats = PeerStats::default();
        assert_in(stats.backoff.next(), secs(10));
        assert_in(stats.backoff.next(), secs(60));
        assert_in(stats.backoff.next(), secs(360));
    }

    #[test]
    fn configured_backoff_is_used_and_reset() {
        let mut stats = PeerStats::new(PeerBackoffConfig {
            min_delay: Duration::from_millis(100),
            factor: 2.,
            max_delay: Duration::from_millis(300),
            total_delay: Some(Duration::from_secs(60)),
        });
        assert_in(stats.backoff.next(), Duration::from_millis(100));
        assert_in(stats.backoff.next(), Duration::from_millis(200));
        // Capped by max_delay.
        assert_in(stats.backoff.next(), Duration::from_millis(300));
        assert_in(stats.backoff.next(), Duration::from_millis(300));
        stats.reset_backoff();
        assert_in(stats.backoff.next(), Duration::from_millis(100));
    }

    #[test]
    fn total_delay_ends_the_backoff() {
        let mut stats = PeerStats::new(PeerBackoffConfig {
            min_delay: secs(1),
            factor: 1.,
            max_delay: secs(1),
            total_delay: Some(secs(5)),
        });
        let mut n = 0;
        while stats.backoff.next().is_some() {
            n += 1;
            assert!(n < 100, "never ends");
        }
        assert!(n >= 1);
    }
}
