use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    io::IoSlice,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context;
use lru::LruCache;
use parking_lot::Mutex;
use tracing::{debug, warn};

use crate::{
    file_info::FileInfo,
    storage::{StorageFactoryExt, filesystem::opened_file::OurFileExt},
    torrent_state::{ManagedTorrentShared, TorrentMetadata},
};

use crate::storage::{StorageFactory, TorrentStorage};

const DEFAULT_FILE_CACHE_CAPACITY: usize = 128;

#[derive(Default, Clone, Copy)]
pub struct FilesystemStorageFactory {}

impl StorageFactory for FilesystemStorageFactory {
    type Storage = FilesystemStorage;

    fn create(
        &self,
        shared: &ManagedTorrentShared,
        _metadata: &TorrentMetadata,
    ) -> anyhow::Result<FilesystemStorage> {
        Ok(FilesystemStorage {
            output_folder: shared.options.output_folder.clone(),
            file_infos: Vec::new(),
            file_cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(DEFAULT_FILE_CACHE_CAPACITY).unwrap(),
            )),
            allow_overwrite: shared.options.allow_overwrite,
        })
    }

    fn clone_box(&self) -> crate::storage::BoxStorageFactory {
        self.boxed()
    }
}

pub struct FilesystemStorage {
    pub(super) output_folder: PathBuf,
    allow_overwrite: bool,
    /// File metadata from torrent. Stored during init() to compute paths lazily
    /// from output_folder + relative_filename on cache miss, avoiding separate
    /// path allocation per file (as suggested by @ikatson).
    file_infos: Vec<FileInfo>,
    /// LRU cache of open file handles, keyed by file_id.
    /// Each entry stores (handle, is_writable) to track access mode.
    /// When a write is requested but the cached handle is read-only,
    /// the stale handle is evicted and a new writable handle is opened.
    file_cache: Mutex<LruCache<usize, (Arc<File>, bool)>>,
}

impl FilesystemStorage {
    pub(super) fn take_fs(&self) -> anyhow::Result<Self> {
        let capacity = {
            let cache = self.file_cache.lock();
            cache.cap()
        };
        Ok(Self {
            output_folder: self.output_folder.clone(),
            allow_overwrite: self.allow_overwrite,
            file_infos: self.file_infos.clone(),
            file_cache: Mutex::new(LruCache::new(capacity)),
        })
    }

    /// Get or open a file handle for the given file_id.
    ///
    /// Uses a two-phase approach to avoid holding the lock during file open:
    /// 1. Check cache under lock → if hit and mode matches, return Arc<File>
    ///    If hit but write requested on a read-only handle, evict the stale entry.
    /// 2. Release lock → open file (blocking) → re-acquire lock → insert
    pub(super) fn get_or_open(&self, file_id: usize, write: bool) -> anyhow::Result<Arc<File>> {
        // Phase 1: check cache
        {
            let mut cache = self.file_cache.lock();
            if let Some((file, is_writable)) = cache.get(&file_id) {
                if !write || *is_writable {
                    // Read request, or write request on a writable handle — cache hit
                    return Ok(Arc::clone(file));
                }
                // Write requested but cached handle is read-only — evict stale entry
                debug!(file_id, "upgrading read-only cached handle to writable");
                cache.pop(&file_id);
            }
        }
        // Cache miss — compute path lazily from file_infos
        let fi = self
            .file_infos
            .get(file_id)
            .context("file_id out of range")?;
        anyhow::ensure!(!fi.attrs.padding, "cannot open padding file");
        let path = self.output_folder.join(&fi.relative_filename);

        let (file, is_writable) = if write {
            let f = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)
                .with_context(|| format!("error opening {path:?} in read/write mode"))?;
            // Mark as sparse file on Windows (once per open, not per write).
            // FSCTL_SET_SPARSE is idempotent but still a syscall — avoid calling it
            // on every I/O operation.
            #[cfg(windows)]
            super::sparse::mark_file_sparse(&f);
            (f, true)
        } else {
            let f = OpenOptions::new()
                .read(true)
                .open(&path)
                .with_context(|| format!("error opening {path:?} in read mode"))?;
            (f, false)
        };

        let file = Arc::new(file);

        // Phase 2: insert into cache under lock
        {
            let mut cache = self.file_cache.lock();
            // Another thread may have inserted while we were opening.
            // Only reuse if the existing handle satisfies the mode requirement.
            if let Some((existing, existing_writable)) = cache.get(&file_id) {
                if !write || *existing_writable {
                    return Ok(Arc::clone(existing));
                }
                // Existing is read-only but we need write — replace it below
                cache.pop(&file_id);
            }
            cache.put(file_id, (Arc::clone(&file), is_writable));
        }

        Ok(file)
    }
}

impl TorrentStorage for FilesystemStorage {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        let file = self.get_or_open(file_id, false)?;
        file.pread_exact(offset, buf)
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        let file = self.get_or_open(file_id, true)?;
        match file.pwrite_all(offset, buf) {
            Ok(()) => Ok(()),
            Err(e) => {
                // On Windows, Access Denied can occur if a stale read-only handle
                // was obtained from the cache (e.g. opened during initial_check).
                // Evict the handle, re-open as writable, and retry once.
                let is_access_denied = e.chain().any(|cause| {
                    cause
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|io_err| io_err.kind() == std::io::ErrorKind::PermissionDenied)
                });
                if !is_access_denied {
                    return Err(e);
                }
                warn!(
                    file_id,
                    offset,
                    size = buf.len(),
                    "pwrite_all: Access Denied, evicting cached handle and retrying"
                );
                {
                    let mut cache = self.file_cache.lock();
                    cache.pop(&file_id);
                }
                drop(file);
                let file = self.get_or_open(file_id, true)?;
                file.pwrite_all(offset, buf)
            }
        }
    }

    fn pwrite_all_vectored(
        &self,
        file_id: usize,
        offset: u64,
        bufs: [IoSlice<'_>; 2],
    ) -> anyhow::Result<usize> {
        let file = self.get_or_open(file_id, true)?;
        match file.pwrite_all_vectored(offset, bufs) {
            Ok(n) => Ok(n),
            Err(e) => {
                let is_access_denied = e.chain().any(|cause| {
                    cause
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|io_err| io_err.kind() == std::io::ErrorKind::PermissionDenied)
                });
                if !is_access_denied {
                    return Err(e);
                }
                warn!(
                    file_id,
                    offset,
                    "pwrite_all_vectored: Access Denied, evicting cached handle and retrying"
                );
                {
                    let mut cache = self.file_cache.lock();
                    cache.pop(&file_id);
                }
                drop(file);
                let file = self.get_or_open(file_id, true)?;
                file.pwrite_all_vectored(offset, bufs)
            }
        }
    }

    fn remove_file(&self, file_id: usize, filename: &Path) -> anyhow::Result<()> {
        // Evict handle from cache before removing file on disk
        {
            let mut cache = self.file_cache.lock();
            cache.pop(&file_id);
        }
        Ok(std::fs::remove_file(self.output_folder.join(filename))?)
    }

    fn ensure_file_length(&self, file_id: usize, len: u64) -> anyhow::Result<()> {
        let file = self.get_or_open(file_id, true)?;
        // Skip set_len if the file already has the correct size.
        // On Windows, File::set_len() calls SetEndOfFile which updates the
        // modification timestamp even when the size is unchanged, causing
        // mtime to reset on every restart for completed torrents.
        if file.metadata()?.len() == len {
            return Ok(());
        }
        Ok(file.set_len(len)?)
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        Ok(Box::new(self.take_fs()?))
    }

    fn remove_directory_if_empty(&self, path: &Path) -> anyhow::Result<()> {
        let path = self.output_folder.join(path);
        if !path.is_dir() {
            anyhow::bail!("cannot remove dir: {path:?} is not a directory")
        }
        if std::fs::read_dir(&path)?.count() == 0 {
            std::fs::remove_dir(&path).with_context(|| format!("error removing {path:?}"))
        } else {
            warn!("did not remove {path:?} as it was not empty");
            Ok(())
        }
    }

    fn init(
        &mut self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        let start = std::time::Instant::now();
        let mut created_dirs: HashSet<PathBuf> = HashSet::new();

        for file_details in metadata.file_infos.iter() {
            if file_details.attrs.padding {
                continue;
            }

            let full_path = self.output_folder.join(&file_details.relative_filename);

            // Deduplicate create_dir_all calls
            if let Some(parent) = full_path.parent() {
                if created_dirs.insert(parent.to_path_buf()) {
                    std::fs::create_dir_all(parent)?;
                }
            }

            // For allow_overwrite=false, use create_new to reject existing files.
            // create_new(true) fails if the file already exists, which is the desired
            // behavior — prevents accidental data loss.
            if !shared.options.allow_overwrite {
                OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&full_path)
                    .with_context(|| {
                        format!(
                            "error creating a new file (because allow_overwrite = false) {:?}",
                            &full_path
                        )
                    })?;
            }
        }

        self.file_infos = metadata.file_infos.clone();

        debug!(
            elapsed = ?start.elapsed(),
            files = metadata.file_infos.len(),
            dirs_created = created_dirs.len(),
            cache_capacity = DEFAULT_FILE_CACHE_CAPACITY,
            "Filesystem storage initialized (lazy file opening)"
        );

        Ok(())
    }
}
