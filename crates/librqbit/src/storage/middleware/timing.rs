/*
A storage middleware that logs the time underlying storage operations took.
*/

use librqbit_core::lengths::ValidPieceIndex;

use crate::{
    ManagedTorrentShared,
    storage::{
        MoveStorageOutcome, MoveStorageRequest, StorageFactory, StorageFactoryExt, TorrentStorage,
    },
    torrent_state::TorrentMetadata,
};

#[derive(Clone)]
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

impl<U: StorageFactory + Clone> StorageFactory for TimingStorageFactory<U> {
    type Storage = TimingStorage<U::Storage>;

    fn create(
        &self,
        shared: &crate::ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<Self::Storage> {
        Ok(TimingStorage {
            name: self.name.clone(),
            underlying: self.underlying_factory.create(shared, metadata)?,
        })
    }

    fn is_type_id(&self, type_id: std::any::TypeId) -> bool {
        self.underlying_factory.is_type_id(type_id)
    }

    fn clone_box(&self) -> crate::storage::BoxStorageFactory {
        self.clone().boxed()
    }
}

pub struct TimingStorage<U> {
    name: String,
    underlying: U,
}

macro_rules! timeit {
    ($name:expr, $op:expr, $($rest:tt),*) => {
        {
            let start = std::time::Instant::now();
            let r = $op;
            let elapsed = start.elapsed();
            tracing::debug!(name = $name, $($rest),*, elapsed_micros=elapsed.as_micros());
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

    fn remove_directory_if_empty(&self, path: &std::path::Path) -> anyhow::Result<()> {
        self.underlying.remove_directory_if_empty(path)
    }

    fn init(
        &mut self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        self.underlying.init(shared, metadata)
    }

    fn on_piece_completed(&self, piece_index: ValidPieceIndex) -> anyhow::Result<()> {
        self.underlying.on_piece_completed(piece_index)
    }

    fn file_length(&self, file_id: usize) -> anyhow::Result<Option<u64>> {
        self.underlying.file_length(file_id)
    }

    fn rename_file(
        &self,
        file_id: usize,
        from: &std::path::Path,
        to: &std::path::Path,
    ) -> anyhow::Result<()> {
        let storage = &self.name;
        timeit!(
            "rename_file",
            self.underlying.rename_file(file_id, from, to),
            file_id,
            storage
        )
    }

    fn move_storage(&self, request: &MoveStorageRequest<'_>) -> anyhow::Result<MoveStorageOutcome> {
        let storage = &self.name;
        timeit!(
            "move_storage",
            self.underlying.move_storage(request),
            storage
        )
    }

    fn finish_move_storage(&self, commit: bool) -> anyhow::Result<()> {
        let storage = &self.name;
        timeit!(
            "finish_move_storage",
            self.underlying.finish_move_storage(commit),
            storage
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_util::{Probe, assert_forwards_defaults};

    #[test]
    fn test_the_defaulted_methods_reach_the_underlying_storage() {
        let storage = TimingStorage {
            name: "test".to_owned(),
            underlying: Probe::default(),
        };
        assert_forwards_defaults(&storage, &storage.underlying);
    }
}
