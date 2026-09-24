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

    /// Whether a torrent kept in this storage can be persisted across a restart - and
    /// if not, what this storage can't promise.
    ///
    /// The persisted record is the torrent, an output folder, a file selection and a
    /// paused flag, plus a have-bitfield beside it. What it doesn't carry is the
    /// storage: on restart the session replays the record through
    /// [`crate::Session::add_torrent`] with no factory of its own, so the torrent comes
    /// back on whatever that session has as its `default_storage_factory`. Two things
    /// follow, and a storage that answers Ok here promises both.
    ///
    /// One: the storage the restart builds addresses the same data as this one. The
    /// filesystem storage gets that for free - the record carries the output folder and
    /// the bytes are laid out as the torrent's own files, so any FilesystemStorage over
    /// that folder finds them again. A storage that keeps a layout of its own has to be
    /// the session's default factory, and the instance the next process builds has to
    /// reach the same data. A factory handed to a single add_torrent call, holding state
    /// nothing outside this process can reconstruct, can't promise this - the torrent
    /// would come back on the session default, which is some other storage entirely.
    ///
    /// Two: the data is still there when the next process asks. The have-bitfield
    /// outlives the process and a restart takes it at its word, checked only by hashing
    /// one piece per file plus at most 64 sampled ones - of a torrent however large. A
    /// storage whose contents go with the process, or that can lose a piece the bitfield
    /// still claims, has rqbit advertising and serving pieces that aren't there.
    ///
    /// The default is no, so that session persistence refuses such a torrent when it is
    /// added rather than discovering it at the next restart. Like [`Self::is_type_id`],
    /// this has to survive wrapping and boxing, and a middleware forwards it to what it
    /// wraps.
    fn ensure_persistable(&self) -> anyhow::Result<()> {
        anyhow::bail!(
            "{} doesn't promise that a restart finds its data again, so a torrent using \
             it can't be persisted. Implement StorageFactory::ensure_persistable if it can.",
            std::any::type_name::<Self>()
        )
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

            fn ensure_persistable(&self) -> anyhow::Result<()> {
                self.sf.ensure_persistable()
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

    fn ensure_persistable(&self) -> anyhow::Result<()> {
        (**self).ensure_persistable()
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
        BoxStorageFactory, StorageFactory, StorageFactoryExt, filesystem::FilesystemStorageFactory,
    };
    use crate::torrent_state::{ManagedTorrentShared, TorrentMetadata};

    // A middleware like the ones in storage::middleware: it wraps another factory and
    // forwards is_type_id, so that what it wraps stays recognizable through it, and
    // passes on what that one promises session persistence.
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

        fn ensure_persistable(&self) -> anyhow::Result<()> {
            self.underlying_factory.ensure_persistable()
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

    // A factory that overrides nothing: it promises session persistence nothing, and the
    // default is what has to say so.
    #[derive(Clone)]
    struct Opaque {}

    impl StorageFactory for Opaque {
        type Storage = Box<dyn super::TorrentStorage>;

        fn create(
            &self,
            _shared: &ManagedTorrentShared,
            _metadata: &TorrentMetadata,
        ) -> anyhow::Result<Self::Storage> {
            anyhow::bail!("not used")
        }

        fn clone_box(&self) -> BoxStorageFactory {
            self.clone().boxed()
        }
    }

    // Whether a storage can be persisted is something it promises, and the promise has to
    // survive the trip to the session, which holds a BoxStorageFactory. Losing it through
    // boxing would refuse torrents that are perfectly persistable; inventing it for a
    // storage that never made it is the expensive direction - resume data outliving the
    // data it describes.
    #[test]
    fn test_ensure_persistable_survives_boxing() {
        assert!(
            FilesystemStorageFactory::default()
                .ensure_persistable()
                .is_ok()
        );
        assert!(
            FilesystemStorageFactory::default()
                .boxed()
                .ensure_persistable()
                .is_ok()
        );

        // Through a middleware, and through the copy clone_box() makes of it.
        let wrapped = Middleware {
            underlying_factory: FilesystemStorageFactory::default(),
        };
        assert!(wrapped.clone().boxed().ensure_persistable().is_ok());
        assert!(wrapped.boxed().clone_box().ensure_persistable().is_ok());

        // The default is no, and it names the factory that didn't promise anything so
        // that whoever added the torrent knows what to fix.
        let err = format!("{:#}", Opaque {}.ensure_persistable().unwrap_err());
        assert!(err.contains("Opaque"), "{err}");
        assert!(err.contains("ensure_persistable"), "{err}");
        assert_eq!(
            format!("{:#}", Opaque {}.boxed().ensure_persistable().unwrap_err()),
            err
        );

        // And a middleware over it doesn't launder it into a yes.
        assert!(
            Middleware {
                underlying_factory: Opaque {},
            }
            .boxed()
            .ensure_persistable()
            .is_err()
        );
    }
}
