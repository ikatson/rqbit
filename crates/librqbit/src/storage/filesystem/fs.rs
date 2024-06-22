use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
};

use anyhow::Context;
use tracing::warn;

use crate::{storage::StorageFactoryExt, torrent_state::ManagedTorrentInfo};

use crate::storage::{StorageFactory, TorrentStorage};

use super::opened_file::OpenedFile;

#[derive(Default, Clone, Copy)]
pub struct FilesystemStorageFactory {}

impl StorageFactory for FilesystemStorageFactory {
    type Storage = FilesystemStorage;

    fn create(&self, meta: &ManagedTorrentInfo) -> anyhow::Result<FilesystemStorage> {
        Ok(FilesystemStorage {
            output_folder: meta.options.output_folder.clone(),
            opened_files: Default::default(),
        })
    }

    fn clone_box(&self) -> crate::storage::BoxStorageFactory {
        self.boxed()
    }
}

pub struct FilesystemStorage {
    pub(super) output_folder: PathBuf,
    pub(super) opened_files: Vec<OpenedFile>,
}

impl FilesystemStorage {
    pub(super) fn take_fs(&self) -> anyhow::Result<Self> {
        Ok(Self {
            opened_files: self
                .opened_files
                .iter()
                .map(|f| f.take_clone())
                .collect::<anyhow::Result<Vec<_>>>()?,
            output_folder: self.output_folder.clone(),
        })
    }
}

impl TorrentStorage for FilesystemStorage {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        let of = self.opened_files.get(file_id).context("no such file")?;
        #[cfg(target_family = "unix")]
        {
            use std::os::unix::fs::FileExt;
            Ok(of.file.read().read_exact_at(buf, offset)?)
        }
        #[cfg(not(target_family = "unix"))]
        {
            use std::io::{Read, Seek, SeekFrom};
            let mut g = of.file.write();
            g.seek(SeekFrom::Start(offset))?;
            Ok(g.read_exact(buf)?)
        }
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        let of = self.opened_files.get(file_id).context("no such file")?;
        of.ensure_writeable()?;
        #[cfg(target_family = "unix")]
        {
            use std::os::unix::fs::FileExt;
            Ok(of.file.read().write_all_at(buf, offset)?)
        }
        #[cfg(target_family = "windows")]
        {
            use std::os::windows::fs::FileExt;
            let mut remaining = buf.len();
            while remaining > 0 {
                remaining -= of.file.read().seek_write(buf, offset)?;
            }
            Ok(())
        }
        #[cfg(not(any(target_family = "unix", target_family = "windows")))]
        {
            use std::io::{Read, Seek, SeekFrom, Write};
            let mut g = of.file.write();
            g.seek(SeekFrom::Start(offset))?;
            Ok(g.write_all(buf)?)
        }
    }

    fn remove_file(&self, _file_id: usize, filename: &Path) -> anyhow::Result<()> {
        Ok(std::fs::remove_file(self.output_folder.join(filename))?)
    }

    fn ensure_file_length(&self, file_id: usize, len: u64) -> anyhow::Result<()> {
        Ok(self.opened_files[file_id].file.write().set_len(len)?)
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        Ok(Box::new(Self {
            opened_files: self
                .opened_files
                .iter()
                .map(|f| f.take_clone())
                .collect::<anyhow::Result<Vec<_>>>()?,
            output_folder: self.output_folder.clone(),
        }))
    }

    fn remove_directory_if_empty(&self, path: &Path) -> anyhow::Result<()> {
        let path = self.output_folder.join(path);
        if !path.is_dir() {
            anyhow::bail!("cannot remove dir: {path:?} is not a directory")
        }
        if std::fs::read_dir(&path)?.next().is_none() {
            std::fs::remove_dir(&path).with_context(|| format!("error removing {path:?}"))
        } else {
            warn!("did not remove {path:?} as it was not empty");
            Ok(())
        }
    }

    fn init(&mut self, meta: &ManagedTorrentInfo) -> anyhow::Result<()> {
        let mut files = Vec::<OpenedFile>::new();
        for file_details in meta.info.iter_file_details(&meta.lengths)? {
            let mut full_path = self.output_folder.clone();
            let relative_path = file_details
                .filename
                .to_pathbuf()
                .context("error converting file to path")?;
            full_path.push(relative_path);

            std::fs::create_dir_all(full_path.parent().context("bug: no parent")?)?;
            if meta.options.allow_overwrite {
                // ensure file exists
                let (file, writeable) = match OpenOptions::new()
                    .create(true)
                    .read(true)
                    .write(true)
                    .append(false)
                    .truncate(false)
                    .open(&full_path)
                {
                    Ok(file) => (file, true),
                    Err(e) => {
                        warn!(?full_path, "error opening file in create+write mode: {e:?}");
                        // open the file in read-only mode, will reopen in write mode later.
                        (
                            OpenOptions::new()
                                .create(false)
                                .read(true)
                                .open(&full_path)
                                .with_context(|| format!("error opening {full_path:?}"))?,
                            false,
                        )
                    }
                };
                files.push(OpenedFile::new(full_path.clone(), file, writeable));
            } else {
                // create_new does not seem to work with read(true), so calling this twice.
                let file = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&full_path)
                    .with_context(|| {
                        format!(
                            "error creating a new file (because allow_overwrite = false) {:?}",
                            &full_path
                        )
                    })?;
                OpenOptions::new().read(true).write(true).open(&full_path)?;
                files.push(OpenedFile::new(full_path.clone(), file, true));
            };
        }

        self.opened_files = files;
        Ok(())
    }
}
