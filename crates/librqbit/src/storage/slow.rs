use std::time::Duration;

use rand::Rng;

use super::{StorageFactory, TorrentStorage};

pub struct SlowStorageFactory<U> {
    underlying_factory: U,
}

impl<U: StorageFactory> SlowStorageFactory<U> {
    pub fn new(underlying: U) -> Self {
        Self {
            underlying_factory: underlying,
        }
    }
}

impl<U: StorageFactory> StorageFactory for SlowStorageFactory<U> {
    fn init_storage(
        &self,
        info: &crate::ManagedTorrentInfo,
    ) -> anyhow::Result<Box<dyn TorrentStorage>> {
        Ok(Box::new(SlowStorage {
            underlying: self.underlying_factory.init_storage(info)?,
        }))
    }
}

struct SlowStorage<U> {
    underlying: U,
}

fn random_sleep() {
    let sl = rand::thread_rng().gen_range(0f64..0.1f64);
    let sl = Duration::from_secs_f64(sl);
    tracing::trace!(duration = ?sl, "sleeping");
    std::thread::sleep(sl)
}

impl<U: TorrentStorage> TorrentStorage for SlowStorage<U> {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        random_sleep();
        self.underlying.pread_exact(file_id, offset, buf)
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        random_sleep();
        self.underlying.pwrite_all(file_id, offset, buf)
    }

    fn remove_file(&self, file_id: usize, filename: &std::path::Path) -> anyhow::Result<()> {
        self.underlying.remove_file(file_id, filename)
    }

    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()> {
        self.underlying.ensure_file_length(file_id, length)
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        Ok(Box::new(SlowStorage {
            underlying: self.underlying.take()?,
        }))
    }
}
