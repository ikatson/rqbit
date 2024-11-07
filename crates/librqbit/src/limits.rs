use std::sync::Arc;
use std::time::Duration;

use leaky_bucket::RateLimiter;
use parking_lot::RwLock;
use peer_binary_protocol::PIECE_MESSAGE_DEFAULT_LEN;
use serde::Deserialize;
use serde::Serialize;

#[derive(Default, Serialize, Deserialize, Clone, Copy)]
pub struct LimitsConfig {
    pub upload_bps: Option<usize>,
    pub download_bps: Option<usize>,
}

struct Limit(RwLock<Arc<Option<leaky_bucket::RateLimiter>>>);

impl Limit {
    fn new_inner(bps: Option<usize>) -> Arc<Option<leaky_bucket::RateLimiter>> {
        let bps = match bps {
            Some(bps) => bps,
            None => return Arc::new(None),
        };
        let b_per_100_ms = bps.div_ceil(10);
        Arc::new(Some(
            RateLimiter::builder()
                .interval(Duration::from_millis(100))
                .refill(b_per_100_ms)
                // whatever the limit is, we need to be able to download / upload a chunk
                .max(PIECE_MESSAGE_DEFAULT_LEN.max(bps))
                .build(),
        ))
    }

    fn new(bps: Option<usize>) -> Self {
        Self(RwLock::new(Self::new_inner(bps)))
    }

    async fn acquire(&self, size: usize) {
        let lim = self.0.read().clone();
        if let Some(rl) = lim.as_ref() {
            rl.acquire(size).await
        }
    }

    fn set(&self, limit: Option<usize>) {
        let new = Self::new_inner(limit);
        *self.0.write() = new;
    }
}

pub struct Limits {
    down: Limit,
    up: Limit,
}

impl Limits {
    pub fn new(config: LimitsConfig) -> Self {
        Self {
            down: Limit::new(config.download_bps),
            up: Limit::new(config.upload_bps),
        }
    }

    pub async fn prepare_for_upload(&self, len: usize) {
        self.up.acquire(len).await
    }

    pub async fn prepare_for_download(&self, len: usize) {
        self.down.acquire(len).await
    }

    pub fn set_upload_bps(&self, bps: Option<usize>) {
        self.up.set(bps);
    }

    pub fn set_download_bps(&self, bps: Option<usize>) {
        self.down.set(bps);
    }
}
