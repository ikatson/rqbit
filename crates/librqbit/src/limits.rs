use std::time::Duration;

use leaky_bucket::RateLimiter;
use peer_binary_protocol::PIECE_MESSAGE_DEFAULT_LEN;
use serde::Deserialize;
use serde::Serialize;

#[derive(Default, Serialize, Deserialize, Clone, Copy)]
pub struct LimitsConfig {
    pub upload_bps: Option<usize>,
    pub download_bps: Option<usize>,
}

#[derive(Default)]
pub struct Limits {
    down: Option<leaky_bucket::RateLimiter>,
    up: Option<leaky_bucket::RateLimiter>,
}

impl Limits {
    pub fn new(config: LimitsConfig) -> Self {
        let new = |bps: usize| -> RateLimiter {
            let b_per_100_ms = bps.div_ceil(10);
            RateLimiter::builder()
                .interval(Duration::from_millis(100))
                .refill(b_per_100_ms)
                // whatever the limit is, we need to be able to download / upload a chunk
                .max(PIECE_MESSAGE_DEFAULT_LEN.max(bps))
                .build()
        };
        Self {
            down: config.download_bps.map(new),
            up: config.upload_bps.map(new),
        }
    }

    pub async fn prepare_for_upload(&self, len: usize) {
        if let Some(rl) = self.up.as_ref() {
            rl.acquire(len).await;
        }
    }

    pub async fn prepare_for_download(&self, len: usize) {
        if let Some(rl) = self.down.as_ref() {
            rl.acquire(len).await;
        }
    }
}
