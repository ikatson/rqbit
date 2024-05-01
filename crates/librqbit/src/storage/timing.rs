use super::{StorageFactory, TorrentStorage};

pub struct TimingStorageFactory {
    name: String,
    underlying_factory: Box<dyn StorageFactory>,
}

impl TimingStorageFactory {
    pub fn new(name: String, underlying: Box<dyn StorageFactory>) -> Self {
        Self {
            name,
            underlying_factory: underlying,
        }
    }
}

impl StorageFactory for TimingStorageFactory {
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

struct TimingStorage {
    name: String,
    underlying: Box<dyn TorrentStorage>,
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

impl TorrentStorage for TimingStorage {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        let storage = &self.name;
        timeit!(
            "pread_exact",
            self.underlying.pread_exact(file_id, offset, buf),
            file_id,
            offset,
            storage
        )
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        let storage = &self.name;
        timeit!(
            "pwrite_all",
            self.underlying.pwrite_all(file_id, offset, buf),
            file_id,
            offset,
            storage
        )
    }

    fn remove_file(&self, file_id: usize, filename: &std::path::Path) -> anyhow::Result<()> {
        self.underlying.remove_file(file_id, filename)
    }

    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()> {
        self.underlying.ensure_file_length(file_id, length)
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        Ok(Box::new(Self {
            underlying: self.underlying.take()?,
            name: self.name.clone(),
        }))
    }
}
