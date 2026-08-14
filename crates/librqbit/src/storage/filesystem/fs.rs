use std::{
    io::IoSlice,
    path::{Path, PathBuf},
};

use anyhow::Context;
use tracing::warn;

use crate::{
    storage::{StorageFactoryExt, filesystem::opened_file::OurFileExt},
    torrent_state::{ManagedTorrentShared, TorrentMetadata},
};

use crate::storage::{StorageFactory, TorrentStorage};

use super::opened_file::{open_file, OpenedFile};

#[derive(Default, Clone, Copy)]
pub struct FilesystemStorageFactory {
    lazy_open: bool,
}

impl FilesystemStorageFactory {
    /// Open backing files on first access instead of all up front at `init`.
    ///
    /// Eager opening makes restoring many/large torrents slow: `init` runs on
    /// the add path and opens every file of every torrent before the session is
    /// usable. With lazy opening, files open on demand (during transfer), and
    /// `ensure_file_length` skips files already at the correct length — so
    /// restoring already-complete torrents opens nothing.
    pub fn with_lazy_open(mut self, lazy_open: bool) -> Self {
        self.lazy_open = lazy_open;
        self
    }
}

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
            lazy_open: self.lazy_open,
        })
    }

    fn clone_box(&self) -> crate::storage::BoxStorageFactory {
        self.boxed()
    }
}

pub struct FilesystemStorage {
    pub(crate) output_folder: PathBuf,
    pub(crate) opened_files: Vec<OpenedFile>,
    pub(crate) lazy_open: bool,
}

impl FilesystemStorage {
    #[allow(dead_code)]
    pub(crate) fn take_fs(&self) -> anyhow::Result<Self> {
        Ok(Self {
            opened_files: self
                .opened_files
                .iter()
                .map(|f| f.take_clone())
                .collect::<anyhow::Result<Vec<_>>>()?,
            output_folder: self.output_folder.clone(),
            lazy_open: self.lazy_open,
        })
    }
}

impl TorrentStorage for FilesystemStorage {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        self.opened_files
            .get(file_id)
            .context("no such file")?
            .lock_read()?
            .pread_exact(offset, buf)
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        let of = self.opened_files.get(file_id).context("no such file")?;
        #[cfg(windows)]
        return of.try_mark_sparse()?.pwrite_all(offset, buf);
        #[cfg(not(windows))]
        return of.lock_read()?.pwrite_all(offset, buf);
    }

    fn pwrite_all_vectored(
        &self,
        file_id: usize,
        offset: u64,
        bufs: [IoSlice<'_>; 2],
    ) -> anyhow::Result<usize> {
        let of = self.opened_files.get(file_id).context("no such file")?;
        #[cfg(windows)]
        return of.try_mark_sparse()?.pwrite_all_vectored(offset, bufs);
        #[cfg(not(windows))]
        return of.lock_read()?.pwrite_all_vectored(offset, bufs);
    }

    fn remove_file(&self, _file_id: usize, filename: &Path) -> anyhow::Result<()> {
        Ok(std::fs::remove_file(self.output_folder.join(filename))?)
    }

    fn ensure_file_length(&self, file_id: usize, len: u64) -> anyhow::Result<()> {
        let f = &self.opened_files.get(file_id).context("no such file")?;
        // Lazy fast path: if the file already exists at the target length, don't
        // open it just to set_len. This lets restoring a complete torrent open
        // none of its files.
        let already_correct = f
            .lazy_unopened_path()
            .and_then(|path| std::fs::metadata(path).ok())
            .map(|m| m.len() == len)
            .unwrap_or(false);
        if already_correct {
            return Ok(());
        }
        #[cfg(windows)]
        f.try_mark_sparse()?;
        Ok(f.lock_read()?.set_len(len)?)
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        Ok(Box::new(Self {
            opened_files: self
                .opened_files
                .iter()
                .map(|f| f.take_clone())
                .collect::<anyhow::Result<Vec<_>>>()?,
            output_folder: self.output_folder.clone(),
            lazy_open: self.lazy_open,
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
        let mut files = Vec::<OpenedFile>::new();
        for file_details in metadata.file_infos.iter() {
            let mut full_path = self.output_folder.clone();
            let relative_path = &file_details.relative_filename;
            full_path.push(relative_path);

            if file_details.attrs.padding {
                files.push(OpenedFile::new_dummy());
                continue;
            };
            if self.lazy_open {
                // Defer create_dir_all + open to first access
                // (see FilesystemStorageFactory::with_lazy_open).
                files.push(OpenedFile::new_lazy(
                    full_path,
                    shared.options.allow_overwrite,
                ));
            } else {
                let f = open_file(&full_path, shared.options.allow_overwrite)?;
                files.push(OpenedFile::new(full_path, f));
            }
        }

        self.opened_files = files;
        Ok(())
    }
}

#[cfg(test)]
mod lazy_tests {
    use super::*;

    fn lazy_storage(files: Vec<OpenedFile>, folder: PathBuf) -> FilesystemStorage {
        FilesystemStorage {
            output_folder: folder,
            opened_files: files,
            lazy_open: true,
        }
    }

    #[test]
    fn lazy_reads_existing_file_only_on_first_access() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.bin");
        std::fs::write(&path, b"hello world").unwrap();

        let of = OpenedFile::new_lazy(path, true);
        assert!(
            of.lazy_unopened_path().is_some(),
            "must not open until accessed"
        );
        let s = lazy_storage(vec![of], dir.path().to_path_buf());

        let mut buf = [0u8; 5];
        s.pread_exact(0, 6, &mut buf).unwrap();
        assert_eq!(&buf, b"world");
        assert!(
            s.opened_files[0].lazy_unopened_path().is_none(),
            "pread should have opened it"
        );
    }

    #[test]
    fn ensure_file_length_is_stat_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.bin");
        std::fs::write(&path, b"1234567890").unwrap(); // len 10

        let s = lazy_storage(
            vec![OpenedFile::new_lazy(path.clone(), true)],
            dir.path().to_path_buf(),
        );

        s.ensure_file_length(0, 10).unwrap();
        assert!(
            s.opened_files[0].lazy_unopened_path().is_some(),
            "correct length must not open the file"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"1234567890");

        s.ensure_file_length(0, 4).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 4);
    }

    #[test]
    fn lazy_pwrite_creates_file_and_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/c.bin");
        let s = lazy_storage(
            vec![OpenedFile::new_lazy(path.clone(), true)],
            dir.path().to_path_buf(),
        );

        s.ensure_file_length(0, 4).unwrap();
        s.pwrite_all(0, 0, b"abcd").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"abcd");
    }
}
