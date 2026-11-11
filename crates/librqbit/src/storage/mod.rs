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
    path::{Path, PathBuf},
};

use librqbit_core::lengths::ValidPieceIndex;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::torrent_state::{ManagedTorrentShared, TorrentMetadata};

/// What [`crate::Session::move_storage`] does with a file of the torrent that already
/// exists at the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveStorageMode {
    /// Refuse the move, before touching anything, if any file of the torrent already
    /// exists at the destination.
    #[default]
    FailIfExists,
    /// Replace what is at the destination with the torrent's file. The replaced file is
    /// kept aside until every file has moved, so a failed move puts it back.
    Overwrite,
    /// Leave a file that already exists at the destination as it is and use it from now
    /// on. The source file is removed only if the destination file holds exactly the same
    /// bytes; otherwise it stays where it was. Because an adopted file that differs may hold
    /// other data, a torrent that adopted such a file is rechecked after the move.
    KeepExisting,
}

/// The arguments of [`TorrentStorage::move_storage`].
pub struct MoveStorageRequest<'a> {
    /// Where the files go. The relative path of each file is kept below it.
    pub new_output_folder: &'a Path,
    /// The relative path of every file of the torrent, by file index, as the storage
    /// knows it today (renames included). `None` for files that are never stored, i.e.
    /// BEP 47 padding files.
    pub files: &'a [Option<PathBuf>],
    pub mode: MoveStorageMode,
    /// Cancelled when the torrent is deleted or the session stops. A storage that sees it
    /// should undo what it moved and return an error.
    pub cancel: &'a CancellationToken,
}

/// What [`TorrentStorage::move_storage`] did.
#[derive(Debug, Default)]
pub struct MoveStorageOutcome {
    /// Files that were already at the destination, differ from the torrent's own (or the
    /// torrent had none) and were kept instead of it (only with
    /// [`MoveStorageMode::KeepExisting`]). Their content is unverified.
    pub adopted_files: Vec<usize>,
}

fn unsupported<T>(operation: &str) -> anyhow::Result<T> {
    anyhow::bail!("{operation} is not supported by this storage")
}

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

    /// The length the data of `file_id` has in the storage right now, or `None` if the
    /// file doesn't exist there. [`crate::AddTorrentOptions::skip_initial_check`] uses it to
    /// see whether the content is complete without reading it.
    ///
    /// The default says the storage can't tell, which makes the torrent hash its data.
    fn file_length(&self, _file_id: usize) -> anyhow::Result<Option<u64>> {
        unsupported("asking for a file's length")
    }

    /// Give file `file_id` a new path, `to`, relative to the output folder. `from` is the
    /// path it has now. Directories `to` needs are created; directories that `from` leaves
    /// empty are removed. A file that doesn't exist yet only changes name: it will be created
    /// at `to`. Must work while I/O on the other files goes on, and leave the file usable
    /// at its new path afterwards (an open handle is closed before the rename and reopened
    /// after it, which Windows needs). On error the file is where it was.
    ///
    /// Used by [`crate::Session::rename_file`].
    fn rename_file(&self, _file_id: usize, _from: &Path, _to: &Path) -> anyhow::Result<()> {
        unsupported("renaming files")
    }

    /// Move every file of the torrent that exists to `request.new_output_folder`, keeping
    /// its relative path, and use the new folder from then on. The torrent does no I/O
    /// while this runs. Must be all or nothing: on error, what was moved is moved back and
    /// the storage keeps working at the old folder.
    ///
    /// A successful move is pending until [`Self::finish_move_storage`]: what it replaced
    /// or left behind (files set aside, sources of adopted files, emptied directories) stays
    /// until then, so that the move can still be undone.
    ///
    /// Used by [`crate::Session::move_storage`], which explains the semantics of
    /// [`MoveStorageMode`].
    fn move_storage(
        &self,
        _request: &MoveStorageRequest<'_>,
    ) -> anyhow::Result<MoveStorageOutcome> {
        unsupported("moving storage")
    }

    /// Ends the move that the last successful [`Self::move_storage`] left pending. With
    /// `commit`, removes what it left behind; otherwise moves every file back and uses the
    /// old folder again (an error then says which files couldn't be put back). Called exactly
    /// once after each successful `move_storage`, before any other I/O.
    fn finish_move_storage(&self, _commit: bool) -> anyhow::Result<()> {
        unsupported("moving storage")
    }
}

impl<U: TorrentStorage + ?Sized> TorrentStorage for Box<U> {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        (**self).pread_exact(file_id, offset, buf)
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        (**self).pwrite_all(file_id, offset, buf)
    }

    fn pwrite_all_vectored(
        &self,
        file_id: usize,
        offset: u64,
        bufs: [IoSlice<'_>; 2],
    ) -> anyhow::Result<usize> {
        (**self).pwrite_all_vectored(file_id, offset, bufs)
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

    fn file_length(&self, file_id: usize) -> anyhow::Result<Option<u64>> {
        (**self).file_length(file_id)
    }

    fn rename_file(&self, file_id: usize, from: &Path, to: &Path) -> anyhow::Result<()> {
        (**self).rename_file(file_id, from, to)
    }

    fn move_storage(&self, request: &MoveStorageRequest<'_>) -> anyhow::Result<MoveStorageOutcome> {
        (**self).move_storage(request)
    }

    fn finish_move_storage(&self, commit: bool) -> anyhow::Result<()> {
        (**self).finish_move_storage(commit)
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
