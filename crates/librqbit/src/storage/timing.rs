use super::{StorageFactory, TorrentStorage};

pub struct TimingStorageFactory<U> {
    name: String,
    underlying_factory: U,
}

impl<U> TimingStorageFactory<U> {
    pub fn new(name: String, underlying: U) -> Self {
        Self {
            name,
            underlying_factory: underlying,
        }
    }
}

impl<U: StorageFactory> StorageFactory for TimingStorageFactory<U> {
    fn init_storage(
        &self,
        info: &crate::ManagedTorrentInfo,
    ) -> anyhow::Result<Box<dyn TorrentStorage>> {
        Ok(Box::new(TimingStorage {
            name: self.name.clone(),
            underlying: self.underlying_factory.init_storage(info)?,
        }))
    }
}

struct TimingStorage<U> {
    name: String,
    underlying: U,
}

macro_rules! timeit {
    ($name:expr, $op:expr, $($rest:tt),*) => {
        {
            let start = std::time::Instant::now();
            let r = $op;
            let elapsed = start.elapsed();
            tracing::debug!(name = $name, $($rest),*, elapsed_micros=elapsed.as_micros(), "timeit");
            r
        }
    };
}

impl<U: TorrentStorage> TorrentStorage for TimingStorage<U> {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        let storage = &self.name;
        let len = buf.len();
        timeit!(
            "pread_exact",
            self.underlying.pread_exact(file_id, offset, buf),
            file_id,
            offset,
            storage,
            len
        )
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        let storage = &self.name;
        let len = buf.len();
        timeit!(
            "pwrite_all",
            self.underlying.pwrite_all(file_id, offset, buf),
            file_id,
            offset,
            storage,
            len
        )
    }

    fn remove_file(&self, file_id: usize, filename: &std::path::Path) -> anyhow::Result<()> {
        self.underlying.remove_file(file_id, filename)
    }

    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()> {
        self.underlying.ensure_file_length(file_id, length)
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        Ok(Box::new(TimingStorage {
            underlying: self.underlying.take()?,
            name: self.name.clone(),
        }))
    }
}
