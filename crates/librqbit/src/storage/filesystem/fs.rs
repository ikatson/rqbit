use std::{
    fs::File,
    io::IoSlice,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context;
use parking_lot::Mutex;
use tracing::{info, warn};

use crate::{
    storage::{StorageFactoryExt, filesystem::opened_file::OurFileExt},
    torrent_state::{ManagedTorrentShared, TorrentMetadata},
};

use crate::storage::{StorageFactory, TorrentStorage};

// We don't use opened_file::OpenedFile anymore, but we use the extension trait.

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
            allow_overwrite: shared.options.allow_overwrite,
            files: Default::default(),
            cache: Arc::new(Mutex::new(FileHandleCache::new(128))),
        })
    }

    fn clone_box(&self) -> crate::storage::BoxStorageFactory {
        self.boxed()
    }
}

struct FileHandleCache {
    limit: usize,
    // Checks for existence in O(1)
    // Value is the file handle.
    // The key is the file index.
    map: std::collections::HashMap<usize, Arc<File>>,
    // LRU queue. Front is MRU, Back is LRU.
    queue: std::collections::VecDeque<usize>,
}

impl FileHandleCache {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            map: Default::default(),
            queue: Default::default(),
        }
    }

    fn get(
        &mut self,
        file_id: usize,
        allow_overwrite: bool,
        path_provider: impl FnOnce() -> anyhow::Result<PathBuf>,
    ) -> anyhow::Result<Arc<File>> {
        if let Some(f) = self.map.get(&file_id).cloned() {
            // Found in cache, promote to MRU
            if let Some(pos) = self.queue.iter().position(|&id| id == file_id) {
                 self.queue.remove(pos);
                 self.queue.push_front(file_id);
            }
            return Ok(f);
        }

        // Not in cache, open it
        let path = path_provider()?;
        
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true).write(true);

        if allow_overwrite {
            opts.create(true).truncate(false);
        } else {
            opts.create_new(true);
        }

        // Try opening read/write, fallback to read-only
        let f = match opts.open(&path) {
            Ok(f) => f,
            Err(e) => {
                let is_access_denied = e.kind() == std::io::ErrorKind::PermissionDenied;
                let raw_os_error = e.raw_os_error();
                let is_sharing_violation = raw_os_error == Some(32);
                let already_exists = e.kind() == std::io::ErrorKind::AlreadyExists;

                if (is_access_denied || is_sharing_violation) && allow_overwrite {
                    warn!("error opening {:?} in read/write mode: {:#}. Trying read-only.", path, e);
                    std::fs::OpenOptions::new().read(true).open(&path).with_context(|| format!("error opening {:?} in read-only mode", path))?
                } else if already_exists && !allow_overwrite {
                     // Try opening existing file if we are not allowed to overwrite but it exists?
                     // Original logic: ensure_new(true) fails if exists.
                     // But if we are resuming? Shared options allow_overwrite is usually TRUE for resume?
                     // If allow_overwrite is FALSE, then we strictly require new file?
                     // Wait, rqbit defaults allow_overwrite=true.
                     return Err(e).with_context(|| format!("error creating new file (allow_overwrite=false) {:?}", path));
                } else {
                    return Err(e).with_context(|| format!("error opening {:?} in read/write mode", path));
                }
            }
        };

        // Mark sparse if windows
        #[cfg(windows)]
        {
            let _ = super::sparse::mark_file_sparse(&f);
        }

        let f = Arc::new(f);

        // Insert into cache
        if self.map.len() >= self.limit {
            if let Some(evicted_id) = self.queue.pop_back() {
                self.map.remove(&evicted_id);
            }
        }
        
        self.map.insert(file_id, f.clone());
        self.queue.push_front(file_id);
        
        Ok(f)
    }
}

pub struct FilesystemStorage {
    pub(super) output_folder: PathBuf,
    allow_overwrite: bool,
    // If None, it's a padding file / dummy.
    pub(super) files: Vec<Option<PathBuf>>,
    cache: Arc<Mutex<FileHandleCache>>,
}

impl FilesystemStorage {
    pub(crate) fn get_file(&self, file_id: usize) -> anyhow::Result<Arc<File>> {
        let path = self.files.get(file_id).context("no such file id")?;
        let path = match path {
             Some(p) => p,
             None => anyhow::bail!("file is padding/dummy"),
        };
        
        // We can just verify existence here if we wanted to be strict, but cache handles open.
        // Wait, cache relies on path_provider closure.
        let mut cache = self.cache.lock();
        cache.get(file_id, self.allow_overwrite, || Ok(path.clone()))
    }

    pub(super) fn take_fs(&self) -> anyhow::Result<Self> {
        Ok(Self {
            output_folder: self.output_folder.clone(),
            allow_overwrite: self.allow_overwrite,
            files: self.files.clone(),
            cache: self.cache.clone(),
        })
    }
}

impl TorrentStorage for FilesystemStorage {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        self.get_file(file_id)?.pread_exact(offset, buf)
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
         self.get_file(file_id)?.pwrite_all(offset, buf)
    }

    fn pwrite_all_vectored(
        &self,
        file_id: usize,
        offset: u64,
        bufs: [IoSlice<'_>; 2],
    ) -> anyhow::Result<usize> {
        self.get_file(file_id)?.pwrite_all_vectored(offset, bufs)
    }

    fn remove_file(&self, _file_id: usize, filename: &Path) -> anyhow::Result<()> {
        Ok(std::fs::remove_file(self.output_folder.join(filename))?)
    }

    fn ensure_file_length(&self, file_id: usize, len: u64) -> anyhow::Result<()> {
        let f = self.get_file(file_id)?;
        Ok(f.set_len(len)?)
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        // Since we use Arc for internal state, a simple clone of the struct words.
        // But we need to implement Clone for FilesystemStorage manually or derive it if fields allow.
        // PathBuf is Clone, Vec is Clone, Arc is Clone.
        Ok(Box::new(FilesystemStorage {
            output_folder: self.output_folder.clone(),
            allow_overwrite: self.allow_overwrite,
            files: self.files.clone(),
            cache: self.cache.clone(),
        }))
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
        use std::collections::HashSet;
        
        info!(output_folder=?self.output_folder, file_count=metadata.file_infos.len(), "initializing filesystem storage");
        let start = std::time::Instant::now();

        if shared.options.kill_locking_processes {
            #[cfg(windows)]
            {
                if let Err(e) = crate::file_locking::kill_processes_locking_path(&self.output_folder, true) {
                    warn!("Error killing locking processes: {:#}", e);
                }
            }
        }

        let mut files = Vec::with_capacity(metadata.file_infos.len());
        let mut created_dirs = HashSet::new();
        // Ensure the root exists
        if let Ok(p) = self.output_folder.canonicalize() {
             created_dirs.insert(p);
        } else {
             // If not exists, create it
             std::fs::create_dir_all(&self.output_folder)?;
             if let Ok(p) = self.output_folder.canonicalize() {
                 created_dirs.insert(p);
             }
        }

        for file_details in metadata.file_infos.iter() {
            let mut full_path = self.output_folder.clone();
            let relative_path = &file_details.relative_filename;
            full_path.push(relative_path);

            if file_details.attrs.padding {
                files.push(None);
                continue;
            };

            // Optimize: check if parent dir already created to avoid 60,000 syscalls
            if let Some(parent) = full_path.parent() {
                 if !created_dirs.contains(parent) {
                      std::fs::create_dir_all(parent).with_context(|| format!("error creating dir {:?}", parent))?;
                      created_dirs.insert(parent.to_path_buf());
                 }
            }
            
            files.push(Some(full_path));
        }

        self.files = files;
        info!(elapsed=?start.elapsed(), "filesystem storage initialized");
        Ok(())
    }
}
