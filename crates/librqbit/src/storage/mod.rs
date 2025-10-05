pub mod filesystem;

#[cfg(feature = "storage_examples")]
pub mod examples;

#[cfg(feature = "storage_middleware")]
pub mod middleware;

use std::{
    any::{Any, TypeId},
    io::IoSlice,
    path::Path,
};

use async_trait::async_trait;
use librqbit_core::lengths::ValidPieceIndex;

use crate::torrent_state::{ManagedTorrentShared, TorrentMetadata};

pub trait StorageFactory: Send + Sync + Any {
    type Storage: TorrentStorage;

    fn create(
        &self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<Self::Storage>;

    fn is_type_id(&self, type_id: TypeId) -> bool {
        Self::type_id(self) == type_id
    }
    fn clone_box(&self) -> BoxStorageFactory;
}

/// Extension trait for async methods on StorageFactory.
#[async_trait]
pub trait StorageFactoryAsyncExt: StorageFactory {
    async fn create_and_init(
        &self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<Self::Storage> {
        let mut storage = self.create(shared, metadata)?;
        storage.init(shared, metadata).await?;
        Ok(storage)
    }
}

impl<T: StorageFactory + ?Sized> StorageFactoryAsyncExt for T {}

pub type BoxStorageFactory = Box<dyn StorageFactory<Storage = Box<dyn TorrentStorage>>>;

pub trait StorageFactoryExt {
    fn boxed(self) -> BoxStorageFactory;
}

impl<SF: StorageFactory> StorageFactoryExt for SF {
    fn boxed(self) -> BoxStorageFactory {
        struct Wrapper<SF> {
            sf: SF,
        }

        impl<SF: StorageFactory> StorageFactory for Wrapper<SF> {
            type Storage = Box<dyn TorrentStorage>;

            fn create(
                &self,
                shared: &ManagedTorrentShared,
                metadata: &TorrentMetadata,
            ) -> anyhow::Result<Self::Storage> {
                let s = self.sf.create(shared, metadata)?;
                Ok(Box::new(s))
            }

            fn is_type_id(&self, type_id: TypeId) -> bool {
                self.sf.type_id() == type_id
            }

            fn clone_box(&self) -> BoxStorageFactory {
                self.sf.clone_box()
            }
        }

        Box::new(Wrapper { sf: self })
    }
}

impl<U: StorageFactory + ?Sized> StorageFactory for Box<U> {
    type Storage = U::Storage;

    fn create(
        &self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<U::Storage> {
        (**self).create(shared, metadata)
    }

    fn clone_box(&self) -> BoxStorageFactory {
        (**self).clone_box()
    }
}

#[async_trait]
pub trait TorrentStorage: Send + Sync {
    // Create/open files etc.
    async fn init(
        &mut self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<()>;

    /// Given a file_id (which you can get more info from in init_storage() through torrent info)
    /// read buf.len() bytes into buf at offset.
    async fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()>;

    /// Given a file_id (which you can get more info from in init_storage() through torrent info)
    /// write buf.len() bytes into the file at offset.
    async fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()>;

    async fn pwrite_all_vectored(
        &self,
        file_id: usize,
        offset: u64,
        bufs: [IoSlice<'_>; 2],
    ) -> anyhow::Result<usize> {
        let mut offset = offset;
        let mut size = 0;

        for ioslice in bufs {
            self.pwrite_all(file_id, offset, &ioslice).await?;
            offset += ioslice.len() as u64;
            size += ioslice.len();
        }

        Ok(size)
    }

    /// Remove a file from the storage. If not supported, or it doesn't matter, just return Ok(())
    async fn remove_file(&self, file_id: usize, filename: &Path) -> anyhow::Result<()>;

    async fn remove_directory_if_empty(&self, path: &Path) -> anyhow::Result<()>;

    /// E.g. for filesystem backend ensure that the file has a certain length, and grow/shrink as needed.
    async fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()>;

    /// Replace the current storage with a dummy, and return a new one that should be used instead.
    /// This is used to make the underlying object useless when e.g. pausing the torrent.
    async fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>>;

    /// Callback called every time a piece has completed and has been validated.
    /// Default implementation does nothing, but can be override in trait implementations.
    async fn on_piece_completed(&self, _piece_index: ValidPieceIndex) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl<U: TorrentStorage + ?Sized> TorrentStorage for Box<U> {
    async fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        (**self).pread_exact(file_id, offset, buf).await
    }

    async fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        (**self).pwrite_all(file_id, offset, buf).await
    }

    async fn remove_file(&self, file_id: usize, filename: &Path) -> anyhow::Result<()> {
        (**self).remove_file(file_id, filename).await
    }

    async fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()> {
        (**self).ensure_file_length(file_id, length).await
    }

    async fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        (**self).take().await
    }

    async fn remove_directory_if_empty(&self, path: &Path) -> anyhow::Result<()> {
        (**self).remove_directory_if_empty(path).await
    }

    async fn init(
        &mut self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        (**self).init(shared, metadata).await
    }

    async fn on_piece_completed(&self, piece_id: ValidPieceIndex) -> anyhow::Result<()> {
        (**self).on_piece_completed(piece_id).await
    }
}
