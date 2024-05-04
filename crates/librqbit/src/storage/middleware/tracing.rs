/*
A storage middleware that logs the time underlying storage operations took.
*/

use librqbit_core::lengths::ValidPieceIndex;

use crate::storage::{StorageFactory, StorageFactoryExt, TorrentStorage};

#[derive(Clone)]
pub struct TracingStorageFactory<U> {
    name: String,
    underlying_factory: U,
}

impl<U> TracingStorageFactory<U> {
    pub fn new(name: String, underlying: U) -> Self {
        Self {
            name,
            underlying_factory: underlying,
        }
    }
}

impl<U: StorageFactory + Clone> StorageFactory for TracingStorageFactory<U> {
    type Storage = TracingStorage<U::Storage>;

    fn init_storage(&self, info: &crate::ManagedTorrentInfo) -> anyhow::Result<Self::Storage> {
        Ok(TracingStorage {
            name: self.name.clone(),
            underlying: self.underlying_factory.init_storage(info)?,
        })
    }

    fn is_type_id(&self, type_id: std::any::TypeId) -> bool {
        self.underlying_factory.is_type_id(type_id)
    }

    fn clone_box(&self) -> crate::storage::BoxStorageFactory {
        self.clone().boxed()
    }
}

pub struct TracingStorage<U> {
    name: String,
    underlying: U,
}

macro_rules! timeit {
    ($name:expr, $op:expr $(, $rest:tt)*) => {
        {
            let start = std::time::Instant::now();
            let r = $op;
            let elapsed = start.elapsed();
            if let Err(e) = &r {
                tracing::debug!(name = $name, error=?e, elapsed_micros=elapsed.as_micros() $(, $rest)*);
            } else {
                tracing::debug!(name = $name, elapsed_micros=elapsed.as_micros() $(, $rest)*);
            }
            r
        }
    };
}

impl<U: TorrentStorage> TorrentStorage for TracingStorage<U> {
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
        let storage = &self.name;
        timeit!(
            "remove_file",
            self.underlying.remove_file(file_id, filename),
            file_id,
            storage
        )
    }

    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()> {
        self.underlying.ensure_file_length(file_id, length)
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        timeit!(
            "take",
            Ok(Box::new(TracingStorage {
                underlying: self.underlying.take()?,
                name: self.name.clone(),
            }) as Box<dyn TorrentStorage>)
        )
    }

    fn flush_piece(&self, piece_id: ValidPieceIndex) -> anyhow::Result<()> {
        self.underlying.flush_piece(piece_id)
    }

    fn discard_piece(&self, piece_id: ValidPieceIndex) -> anyhow::Result<()> {
        self.underlying.discard_piece(piece_id)
    }
}