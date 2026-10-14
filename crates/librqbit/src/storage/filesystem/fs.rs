use std::{
    collections::VecDeque,
    fs::{File, OpenOptions},
    io::IoSlice,
    path::{Path, PathBuf},
};

use anyhow::Context;
use parking_lot::Mutex;
use tracing::warn;

use crate::{
    storage::{StorageFactoryExt, filesystem::opened_file::OurFileExt},
    torrent_state::{ManagedTorrentShared, TorrentMetadata},
};

use crate::storage::{StorageFactory, TorrentStorage};

use super::opened_file::OpenedFile;

// Each torrent keeps at most this many idle/active file descriptors in its
// cache. In-flight writes hold a read guard, so evicting a busy file waits
// for that operation to finish instead of closing a descriptor mid-write.
const MAX_CACHED_FILES: usize = 128;

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
            opened_files: Default::default(),
            recent_files: Default::default(),
        })
    }

    fn clone_box(&self) -> crate::storage::BoxStorageFactory {
        self.boxed()
    }
}

pub struct FilesystemStorage {
    pub(crate) output_folder: PathBuf,
    pub(crate) opened_files: Vec<OpenedFile>,
    recent_files: Mutex<VecDeque<usize>>,
}

impl FilesystemStorage {
    /// Opens only the files used by the current transfer. Cache bookkeeping is
    /// serialized, but the actual reads and writes run concurrently.
    pub(crate) fn with_file<T>(
        &self,
        file_id: usize,
        _mark_sparse: bool,
        operation: impl FnOnce(&File) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let file = self.opened_files.get(file_id).context("no such file")?;
        let mut recent = self.recent_files.lock();

        if let Some(index) = recent.iter().position(|&id| id == file_id) {
            recent.remove(index);
        } else if recent.len() >= MAX_CACHED_FILES {
            if let Some(oldest) = recent.pop_front() {
                self.opened_files[oldest].close();
            }
        }

        file.open_if_needed()?;
        recent.push_back(file_id);

        #[cfg(windows)]
        if _mark_sparse {
            // A reopened descriptor must preserve the existing Windows sparse
            // file behaviour before any write or set_len operation.
            let guard = file.try_mark_sparse()?;
            drop(recent);
            return operation(&guard);
        }

        let guard = file.lock_read()?;
        drop(recent);
        operation(&guard)
    }

    #[allow(dead_code)]
    pub(crate) fn take_fs(&self) -> anyhow::Result<Self> {
        // The previous storage is no longer used after a take(). Drop cached
        // descriptors on the receiver so it can rebuild an accurate LRU.
        let mut recent = self.recent_files.lock();
        let files = self
            .opened_files
            .iter()
            .map(|f| f.take_clone())
            .collect::<anyhow::Result<Vec<_>>>()?;
        for file in &files {
            file.close();
        }
        recent.clear();

        Ok(Self {
            opened_files: files,
            recent_files: Default::default(),
            output_folder: self.output_folder.clone(),
        })
    }
}

impl TorrentStorage for FilesystemStorage {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        self.with_file(file_id, false, |file| file.pread_exact(offset, buf))
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        self.with_file(file_id, true, |file| file.pwrite_all(offset, buf))
    }

    fn pwrite_all_vectored(
        &self,
        file_id: usize,
        offset: u64,
        bufs: [IoSlice<'_>; 2],
    ) -> anyhow::Result<usize> {
        self.with_file(file_id, true, |file| file.pwrite_all_vectored(offset, bufs))
    }

    fn remove_file(&self, file_id: usize, filename: &Path) -> anyhow::Result<()> {
        // Windows cannot delete a file while our cache still holds it open.
        {
            let mut recent = self.recent_files.lock();
            if let Some(index) = recent.iter().position(|&id| id == file_id) {
                recent.remove(index);
                self.opened_files[file_id].close();
            }
        }
        Ok(std::fs::remove_file(self.output_folder.join(filename))?)
    }

    fn ensure_file_length(&self, file_id: usize, len: u64) -> anyhow::Result<()> {
        self.with_file(file_id, true, |file| Ok(file.set_len(len)?))
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
        let mut files = Vec::<OpenedFile>::new();
        for file_details in metadata.file_infos.iter() {
            let mut full_path = self.output_folder.clone();
            let relative_path = &file_details.relative_filename;
            full_path.push(relative_path);

            if file_details.attrs.padding {
                files.push(OpenedFile::new_dummy());
                continue;
            };
            std::fs::create_dir_all(full_path.parent().context("bug: no parent")?)?;
            let f = if shared.options.allow_overwrite {
                OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(&full_path)
                    .with_context(|| format!("error opening {full_path:?} in read/write mode"))?
            } else {
                // create_new does not seem to work with read(true), so calling this twice.
                OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&full_path)
                    .with_context(|| {
                        format!(
                            "error creating a new file (because allow_overwrite = false) {:?}",
                            full_path
                        )
                    })?;
                OpenOptions::new().read(true).write(true).open(&full_path)?
            };
            // Avoid holding one descriptor for every member of a multi-file
            // torrent. Some small torrents contain tens of thousands of files.
            drop(f);
            files.push(OpenedFile::new_closed(full_path));
        }

        self.recent_files.lock().clear();
        self.opened_files = files;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{FilesystemStorage, MAX_CACHED_FILES, OpenedFile};
    use crate::storage::TorrentStorage;
    use parking_lot::Mutex;
    use std::{collections::VecDeque, sync::Arc};

    fn many_file_storage(count: usize) -> (tempfile::TempDir, FilesystemStorage) {
        let temp = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        for id in 0..count {
            let path = temp.path().join(format!("file_{id:05}.bin"));
            std::fs::write(&path, [id as u8]).unwrap();
            files.push(OpenedFile::new_closed(path));
        }
        let storage = FilesystemStorage {
            output_folder: temp.path().to_path_buf(),
            opened_files: files,
            recent_files: Mutex::new(VecDeque::new()),
        };
        (temp, storage)
    }

    #[test]
    fn thousands_of_files_do_not_exhaust_descriptors() {
        let (_temp, storage) = many_file_storage(1024);
        for id in 0..1024 {
            let mut result = [0u8; 1];
            storage.pread_exact(id, 0, &mut result).unwrap();
            assert_eq!(result, [id as u8]);
        }

        assert_eq!(storage.recent_files.lock().len(), MAX_CACHED_FILES);
        assert_eq!(
            storage
                .opened_files
                .iter()
                .filter(|file| file.lock_read().is_ok())
                .count(),
            MAX_CACHED_FILES
        );

        storage.pwrite_all(0, 0, &[123]).unwrap();
        let mut result = [0u8; 1];
        storage.pread_exact(0, 0, &mut result).unwrap();
        assert_eq!(result, [123]);
    }

    #[test]
    fn concurrent_io_with_eviction_keeps_data_intact() {
        let (_temp, storage) = many_file_storage(300);
        let storage = Arc::new(storage);
        std::thread::scope(|scope| {
            for worker in 0..4 {
                let storage = storage.clone();
                scope.spawn(move || {
                    for id in (worker..300).step_by(4) {
                        storage.pwrite_all(id, 0, &[255 - id as u8]).unwrap();
                        let mut value = [0u8; 1];
                        storage.pread_exact(id, 0, &mut value).unwrap();
                        assert_eq!(value, [255 - id as u8]);
                    }
                });
            }
        });
        assert!(storage.recent_files.lock().len() <= MAX_CACHED_FILES);
    }
}
