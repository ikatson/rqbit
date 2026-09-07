//! Storage engine for torrent data.
//!
//! This is deliberately sync, not async by design, for several reasons.
//!
//! Reason 1. Performance: avoiding copying.
//!
//! Torrent files are large so memcpy costs can compound. Tokio FS does all file writes in a thread
//! pool. To do those writes it has a buffer per file. When you call e.g. "write", your request is first
//! memcpy'ed to that buffer, and only then written to the file.
//!
//! On the write path (download), we write straight from the peer's socket buffer into the file.
//! On the read path (upload), we read straight into the peer's socket buffer also.
//!
//! Reason 2. Memory use: memory bloat would be a problem if tokio::fs was used.
//!
//! The said buffers above default to 2MB. We have a lot of files open, so this can compound into a pretty large
//! memory use.
//!
//! Reason 3. Performance: advanced FS APIs.
//!
//! We use positioned vectored writes (pwritev). Tokio doesn't support that.
//! Positioned so that writing can be done to files in parallel without locks.
//! Vectored so that we issue 1 write call for a potentially non-contiguous chunk.

pub mod filesystem;

#[cfg(feature = "storage_examples")]
pub mod examples;

#[cfg(feature = "storage_middleware")]
pub mod middleware;

#[cfg(test)]
pub(crate) mod test_util;

use std::{
    any::{Any, TypeId},
    io::IoSlice,
    path::Path,
};

use librqbit_core::lengths::ValidPieceIndex;

use crate::torrent_state::{ManagedTorrentShared, TorrentMetadata};

pub trait StorageFactory: Send + Sync + Any {
    type Storage: TorrentStorage;

    fn create(
        &self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<Self::Storage>;
    fn create_and_init(
        &self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<Self::Storage> {
        let mut storage = self.create(shared, metadata)?;
        storage.init(shared, metadata)?;
        Ok(storage)
    }

    /// Whether this factory, or the one it ultimately wraps, is of the given type.
    ///
    /// It has to survive both wrapping and boxing: this is how e.g. session persistence
    /// asks whether it's dealing with a FilesystemStorageFactory, and a middleware in
    /// between must not hide it. A middleware forwards this to what it wraps.
    fn is_type_id(&self, type_id: TypeId) -> bool {
        Self::type_id(self) == type_id
    }
    fn clone_box(&self) -> BoxStorageFactory;
}

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
                self.sf.is_type_id(type_id)
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

    fn is_type_id(&self, type_id: TypeId) -> bool {
        (**self).is_type_id(type_id)
    }

    fn clone_box(&self) -> BoxStorageFactory {
        (**self).clone_box()
    }
}

pub trait TorrentStorage: Send + Sync {
    // Create/open files etc.
    fn init(
        &mut self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<()>;

    /// Given a file_id (which you can get more info from in init_storage() through torrent info)
    /// read buf.len() bytes into buf at offset.
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()>;

    /// Given a file_id (which you can get more info from in init_storage() through torrent info)
    /// write buf.len() bytes into the file at offset.
    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()>;

    fn pwrite_all_vectored(
        &self,
        file_id: usize,
        offset: u64,
        bufs: [IoSlice<'_>; 2],
    ) -> anyhow::Result<usize> {
        let mut offset = offset;
        let mut size = 0;

        for ioslice in bufs {
            self.pwrite_all(file_id, offset, &ioslice)?;
            offset += ioslice.len() as u64;
            size += ioslice.len();
        }

        Ok(size)
    }

    /// Remove a file from the storage. If not supported, or it doesn't matter, just return Ok(())
    fn remove_file(&self, file_id: usize, filename: &Path) -> anyhow::Result<()>;

    fn remove_directory_if_empty(&self, path: &Path) -> anyhow::Result<()>;

    /// E.g. for filesystem backend ensure that the file has a certain length, and grow/shrink as needed.
    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()>;

    /// Replace the current storage with a dummy, and return a new one that should be used instead.
    /// This is used to make the underlying object useless when e.g. pausing the torrent.
    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>>;

    /// Callback called every time a piece has completed and has been validated.
    /// Default implementation does nothing, but can be override in trait implementations.
    fn on_piece_completed(&self, _piece_index: ValidPieceIndex) -> anyhow::Result<()> {
        Ok(())
    }
}

impl<U: TorrentStorage + ?Sized> TorrentStorage for Box<U> {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        (**self).pread_exact(file_id, offset, buf)
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        (**self).pwrite_all(file_id, offset, buf)
    }

    fn remove_file(&self, file_id: usize, filename: &Path) -> anyhow::Result<()> {
        (**self).remove_file(file_id, filename)
    }

    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()> {
        (**self).ensure_file_length(file_id, length)
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        (**self).take()
    }

    fn remove_directory_if_empty(&self, path: &Path) -> anyhow::Result<()> {
        (**self).remove_directory_if_empty(path)
    }

    fn init(
        &mut self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        (**self).init(shared, metadata)
    }

    fn on_piece_completed(&self, piece_id: ValidPieceIndex) -> anyhow::Result<()> {
        (**self).on_piece_completed(piece_id)
    }
}

#[cfg(test)]
mod tests {
    use std::any::TypeId;

    use super::{
        BoxStorageFactory, StorageFactory, StorageFactoryExt,
        filesystem::FilesystemStorageFactory,
        test_util::{Probe, assert_forwards_defaults},
    };
    use crate::torrent_state::{ManagedTorrentShared, TorrentMetadata};

    // A middleware like the ones in storage::middleware: it wraps another factory and
    // forwards is_type_id, so that what it wraps stays recognizable through it.
    #[derive(Clone)]
    struct Middleware<U> {
        underlying_factory: U,
    }

    impl<U: StorageFactory + Clone> StorageFactory for Middleware<U> {
        type Storage = U::Storage;

        fn create(
            &self,
            shared: &ManagedTorrentShared,
            metadata: &TorrentMetadata,
        ) -> anyhow::Result<Self::Storage> {
            self.underlying_factory.create(shared, metadata)
        }

        fn is_type_id(&self, type_id: TypeId) -> bool {
            self.underlying_factory.is_type_id(type_id)
        }

        fn clone_box(&self) -> BoxStorageFactory {
            self.clone().boxed()
        }
    }

    #[test]
    fn test_is_type_id_survives_boxing() {
        let fs = TypeId::of::<FilesystemStorageFactory>();

        assert!(FilesystemStorageFactory::default().is_type_id(fs));
        // Box<U> has to forward, or the answer is the TypeId of the box itself.
        assert!(FilesystemStorageFactory::default().boxed().is_type_id(fs));

        // And boxed() has to forward too, or every middleware's override is lost and the
        // answer is the middleware's own type.
        let wrapped = Middleware {
            underlying_factory: FilesystemStorageFactory::default(),
        };
        assert!(wrapped.is_type_id(fs));
        assert!(wrapped.clone().boxed().is_type_id(fs));
        assert!(wrapped.boxed().clone_box().is_type_id(fs));

        // A factory that isn't the one asked about still says so.
        assert!(
            !Middleware {
                underlying_factory: FilesystemStorageFactory::default(),
            }
            .boxed()
            .is_type_id(TypeId::of::<Middleware<FilesystemStorageFactory>>())
        );
    }

    // What a torrent holds is a Box<dyn TorrentStorage>, so a method this impl doesn't
    // forward is one no storage in rqbit ever gets asked.
    #[test]
    fn test_the_defaulted_methods_survive_boxing() {
        let boxed: Box<Probe> = Box::default();
        assert_forwards_defaults(&boxed, &boxed);
    }
}
