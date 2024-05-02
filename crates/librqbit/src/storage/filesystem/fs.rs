use std::{
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use anyhow::Context;

use crate::{storage::StorageFactoryExt, torrent_state::ManagedTorrentInfo};

use crate::storage::{StorageFactory, TorrentStorage};

use super::opened_file::OpenedFile;

#[derive(Default, Clone, Copy)]
pub struct FilesystemStorageFactory {}

impl StorageFactory for FilesystemStorageFactory {
    type Storage = FilesystemStorage;

    fn init_storage(&self, meta: &ManagedTorrentInfo) -> anyhow::Result<FilesystemStorage> {
        let mut files = Vec::<OpenedFile>::new();
        let output_folder = &meta.options.output_folder;
        for file_details in meta.info.iter_file_details(&meta.lengths)? {
            let mut full_path = output_folder.clone();
            let relative_path = file_details
                .filename
                .to_pathbuf()
                .context("error converting file to path")?;
            full_path.push(relative_path);

            std::fs::create_dir_all(full_path.parent().context("bug: no parent")?)?;
            let file = if meta.options.allow_overwrite {
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
                            &full_path
                        )
                    })?;
                OpenOptions::new().read(true).write(true).open(&full_path)?
            };
            files.push(OpenedFile::new(file));
        }
        Ok(FilesystemStorage {
            output_folder: output_folder.clone(),
            opened_files: files,
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
        let mut g = self
            .opened_files
            .get(file_id)
            .context("no such file")?
            .file
            .lock();
        g.seek(SeekFrom::Start(offset))?;
        Ok(g.read_exact(buf)?)
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        let mut g = self
            .opened_files
            .get(file_id)
            .context("no such file")?
            .file
            .lock();
        g.seek(SeekFrom::Start(offset))?;
        Ok(g.write_all(buf)?)
    }

    fn remove_file(&self, _file_id: usize, filename: &Path) -> anyhow::Result<()> {
        Ok(std::fs::remove_file(self.output_folder.join(filename))?)
    }

    fn ensure_file_length(&self, file_id: usize, len: u64) -> anyhow::Result<()> {
        Ok(self.opened_files[file_id].file.lock().set_len(len)?)
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
}
