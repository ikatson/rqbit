use arc_swap::ArcSwapOption;
use governor::DefaultDirectRateLimiter as RateLimiter;
use governor::Quota;
use serde::Deserialize;
use serde::Serialize;
use std::num::NonZeroU32;
use std::sync::Arc;
use tokio::sync::Notify;

/// Upload and download rates in bytes per second. `None` is unlimited.
#[derive(Default, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimitsConfig {
    pub upload_bps: Option<NonZeroU32>,
    pub download_bps: Option<NonZeroU32>,
}

// A token bucket that holds one second worth of the rate.
struct Bucket {
    limiter: RateLimiter,
    bps: NonZeroU32,
}

struct Limit {
    bucket: ArcSwapOption<Bucket>,
    // Wakes the transfers that wait on the bucket when it is replaced, so that they go on at
    // the new rate instead of serving out the wait the old one gave them.
    changed: Notify,
}

impl Limit {
    fn new_bucket(bps: Option<NonZeroU32>) -> Option<Arc<Bucket>> {
        let bps = bps?;
        Some(Arc::new(Bucket {
            limiter: RateLimiter::direct(Quota::per_second(bps)),
            bps,
        }))
    }

    fn new(bps: Option<NonZeroU32>) -> Self {
        Self {
            bucket: ArcSwapOption::new(Self::new_bucket(bps)),
            changed: Notify::new(),
        }
    }

    async fn acquire(&self, size: NonZeroU32) -> crate::Result<()> {
        let mut left = size.get();
        while let Some(want) = NonZeroU32::new(left) {
            // Registered before the bucket is read: a change made after the read wakes us up.
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();

            let Some(bucket) = self.bucket.load_full() else {
                return Ok(());
            };
            // The bucket never holds more than a second of the rate, so a transfer larger than
            // that (a block at a rate below 16 KiB/s) is let through in parts.
            let part = want.min(bucket.bps);
            tokio::select! {
                res = bucket.limiter.until_n_ready(part) => {
                    res?;
                    left -= part.get();
                }
                // The rate changed: what is left is paced by the new bucket.
                _ = &mut changed => {}
            }
        }
        Ok(())
    }

    fn set(&self, limit: Option<NonZeroU32>) {
        // An unchanged rate keeps its bucket: a new one would start full and hand out a fresh
        // second worth of the rate.
        if self.get() == limit {
            return;
        }
        self.bucket.store(Self::new_bucket(limit));
        self.changed.notify_waiters();
    }

    fn get(&self) -> Option<NonZeroU32> {
        self.bucket.load().as_ref().map(|b| b.bps)
    }
}

/// Upload and download rate limits that can be changed at any time. A change applies at once,
/// including to the transfers that are waiting for the old rate.
pub struct Limits {
    down: Limit,
    up: Limit,
}

impl Default for Limits {
    fn default() -> Self {
        Self::new(LimitsConfig::default())
    }
}

impl Limits {
    pub fn new(config: LimitsConfig) -> Self {
        Self {
            down: Limit::new(config.download_bps),
            up: Limit::new(config.upload_bps),
        }
    }

    pub async fn prepare_for_upload(&self, len: NonZeroU32) -> crate::Result<()> {
        self.up.acquire(len).await
    }

    pub async fn prepare_for_download(&self, len: NonZeroU32) -> crate::Result<()> {
        self.down.acquire(len).await
    }

    pub fn set_upload_bps(&self, bps: Option<NonZeroU32>) {
        self.up.set(bps);
    }

    pub fn set_download_bps(&self, bps: Option<NonZeroU32>) {
        self.down.set(bps);
    }

    /// Sets both directions.
    pub fn set_config(&self, config: LimitsConfig) {
        self.set_upload_bps(config.upload_bps);
        self.set_download_bps(config.download_bps);
    }

    pub fn get_upload_bps(&self) -> Option<NonZeroU32> {
        self.up.get()
    }

    pub fn get_download_bps(&self) -> Option<NonZeroU32> {
        self.down.get()
    }

    pub fn get_config(&self) -> LimitsConfig {
        LimitsConfig {
            upload_bps: self.get_upload_bps(),
            download_bps: self.get_download_bps(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroU32, sync::Arc, time::Duration};

    use tokio::time::{Instant, timeout};

    use super::{Limits, LimitsConfig};

    fn nz(v: u32) -> Option<NonZeroU32> {
        NonZeroU32::new(v)
    }

    #[tokio::test]
    async fn test_transfer_larger_than_the_rate_is_paced_not_refused() {
        // 1 KiB/s, a 2 KiB transfer: the first KiB is in the bucket, the second takes a second.
        let limits = Limits::new(LimitsConfig {
            upload_bps: nz(1024),
            download_bps: None,
        });
        let start = Instant::now();
        timeout(
            Duration::from_secs(10),
            limits.prepare_for_upload(NonZeroU32::new(2048).unwrap()),
        )
        .await
        .expect("timed out")
        .expect("a transfer larger than the rate must not fail");
        assert!(start.elapsed() >= Duration::from_millis(900));
    }

    #[tokio::test]
    async fn test_waiters_follow_a_raised_rate() {
        let limits = Arc::new(Limits::new(LimitsConfig {
            upload_bps: None,
            download_bps: nz(1024),
        }));
        // Empties the bucket: what comes next waits for the rate.
        limits
            .prepare_for_download(NonZeroU32::new(1024).unwrap())
            .await
            .unwrap();
        // 60 seconds at the old rate.
        let waiter = tokio::spawn({
            let limits = limits.clone();
            async move {
                limits
                    .prepare_for_download(NonZeroU32::new(60 * 1024).unwrap())
                    .await
            }
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());

        limits.set_download_bps(nz(1024 * 1024));
        timeout(Duration::from_secs(10), waiter)
            .await
            .expect("the waiter served out the old rate")
            .unwrap()
            .unwrap();
        assert_eq!(limits.get_download_bps(), nz(1024 * 1024));
    }

    #[tokio::test]
    async fn test_waiters_are_released_when_the_limit_is_removed() {
        let limits = Arc::new(Limits::new(LimitsConfig {
            upload_bps: nz(1024),
            download_bps: None,
        }));
        limits
            .prepare_for_upload(NonZeroU32::new(1024).unwrap())
            .await
            .unwrap();
        let waiters: Vec<_> = (0..4)
            .map(|_| {
                let limits = limits.clone();
                tokio::spawn(async move {
                    limits
                        .prepare_for_upload(NonZeroU32::new(16 * 1024).unwrap())
                        .await
                })
            })
            .collect();
        tokio::task::yield_now().await;

        limits.set_config(LimitsConfig::default());
        assert_eq!(limits.get_config(), LimitsConfig::default());
        for w in waiters {
            timeout(Duration::from_secs(10), w)
                .await
                .expect("a waiter kept the old rate")
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn test_waiters_follow_a_lowered_rate() {
        let limits = Arc::new(Limits::new(LimitsConfig {
            upload_bps: nz(64 * 1024),
            download_bps: None,
        }));
        limits
            .prepare_for_upload(NonZeroU32::new(64 * 1024).unwrap())
            .await
            .unwrap();
        // Half a second at the old rate, 16 seconds at the new one.
        let waiter = tokio::spawn({
            let limits = limits.clone();
            async move {
                limits
                    .prepare_for_upload(NonZeroU32::new(32 * 1024).unwrap())
                    .await
            }
        });
        tokio::task::yield_now().await;
        limits.set_upload_bps(nz(1024));
        // The new bucket starts full (one KiB), the remaining 31 KiB need 31 seconds.
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(
            !waiter.is_finished(),
            "the waiter kept the old, higher rate"
        );
        waiter.abort();
    }
}
