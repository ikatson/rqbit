use governor::DefaultDirectRateLimiter as RateLimiter;
use governor::Quota;
use parking_lot::RwLock;
use serde::Deserialize;
use serde::Serialize;
use std::num::NonZero;
use std::num::NonZeroU32;
use std::sync::Arc;

#[derive(Default, Serialize, Deserialize, Clone, Copy)]
pub struct LimitsConfig {
    pub upload_bps: Option<NonZero<u32>>,
    pub download_bps: Option<NonZero<u32>>,
}

struct Limit(RwLock<Option<Arc<RateLimiter>>>);

impl Limit {
    fn new_inner(bps: Option<NonZero<u32>>) -> Option<Arc<RateLimiter>> {
        let bps = bps?;
        Some(Arc::new(RateLimiter::direct(Quota::per_second(bps))))
    }

    fn new(bps: Option<NonZero<u32>>) -> Self {
        Self(RwLock::new(Self::new_inner(bps)))
    }

    async fn acquire(&self, size: NonZero<u32>) -> anyhow::Result<()> {
        let lim = self.0.read().clone();
        if let Some(rl) = lim.as_ref() {
            rl.until_n_ready(size).await?;
        }
        Ok(())
    }

    fn set(&self, limit: Option<NonZero<u32>>) {
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

    pub async fn prepare_for_upload(&self, len: NonZero<u32>) -> anyhow::Result<()> {
        self.up.acquire(len).await
    }

    pub async fn prepare_for_download(&self, len: NonZero<u32>) -> anyhow::Result<()> {
        self.down.acquire(len).await
    }

    pub fn set_upload_bps(&self, bps: Option<NonZero<u32>>) {
        self.up.set(bps);
    }

    pub fn set_download_bps(&self, bps: Option<NonZeroU32>) {
        self.down.set(bps);
    }
}
