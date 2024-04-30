use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use anyhow::Context;
use librqbit_core::lengths::{Lengths, ValidPieceIndex};
use parking_lot::RwLock;

use crate::{opened_file::OpenedFile, torrent_state::ManagedTorrentInfo, type_aliases::FileInfos};

pub trait StorageFactory: Send + Sync {
    fn init_storage(&self, info: &ManagedTorrentInfo) -> anyhow::Result<Box<dyn TorrentStorage>>;

    fn output_folder(&self) -> Option<&Path> {
        None
    }
}

pub trait TorrentStorage: Send + Sync {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()>;

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()>;

    fn remove_file(&self, file_id: usize, filename: &Path) -> anyhow::Result<()>;

    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()>;

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>>;

    fn output_folder(&self) -> Option<&Path> {
        None
    }
}

pub struct FilesystemStorageFactory {
    pub output_folder: PathBuf,
    pub allow_overwrite: bool,
}

impl StorageFactory for FilesystemStorageFactory {
    fn init_storage(&self, meta: &ManagedTorrentInfo) -> anyhow::Result<Box<dyn TorrentStorage>> {
        let mut files = Vec::<OpenedFile>::new();
        for file_details in meta.info.iter_file_details(&meta.lengths)? {
            let mut full_path = self.output_folder.clone();
            let relative_path = file_details
                .filename
                .to_pathbuf()
                .context("error converting file to path")?;
            full_path.push(relative_path);

            std::fs::create_dir_all(full_path.parent().context("bug: no parent")?)?;
            let file = if self.allow_overwrite {
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
                    .with_context(|| format!("error creating {:?}", &full_path))?;
                OpenOptions::new().read(true).write(true).open(&full_path)?
            };
            files.push(OpenedFile::new(file));
        }
        Ok(Box::new(FilesystemStorage {
            output_folder: self.output_folder.clone(),
            opened_files: files,
        }))
    }

    fn output_folder(&self) -> Option<&Path> {
        Some(&self.output_folder)
    }
}

pub struct FilesystemStorage {
    output_folder: PathBuf,
    opened_files: Vec<OpenedFile>,
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

impl TorrentStorage for Box<dyn TorrentStorage> {
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
}

struct InMemoryPiece {
    bytes: Box<[u8]>,
}

impl InMemoryPiece {
    fn new(l: &Lengths) -> Self {
        let v = vec![0; l.default_piece_length() as usize].into_boxed_slice();
        Self { bytes: v }
    }
}

pub struct InMemoryGarbageCollectingStorage {
    lengths: Lengths,
    file_infos: FileInfos,
    map: RwLock<HashMap<ValidPieceIndex, InMemoryPiece>>,
    // TODO: chunk tracker - rename to PieceTracker and extract chunks out of it (only keep pieces)
    // this sucker here would track chunks, and the storage above too.
}

impl InMemoryGarbageCollectingStorage {
    pub fn new(lengths: Lengths, file_infos: FileInfos) -> anyhow::Result<Self> {
        // Max memory 128MiB. Make it tunable
        let max_pieces = 128 * 1024 * 1024 / lengths.default_piece_length();
        if max_pieces == 0 {
            anyhow::bail!("pieces too large");
        }

        Ok(Self {
            lengths,
            file_infos,
            map: RwLock::new(HashMap::new()),
        })
    }
}

impl TorrentStorage for InMemoryGarbageCollectingStorage {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        let fi = &self.file_infos[file_id];
        let abs_offset = fi.offset_in_torrent + offset;
        let piece_id: u32 = (abs_offset / self.lengths.default_piece_length() as u64).try_into()?;
        let piece_offset: usize =
            (abs_offset % self.lengths.default_piece_length() as u64).try_into()?;
        let piece_id = self.lengths.validate_piece_index(piece_id).context("bug")?;

        let g = self.map.read();
        let inmp = g.get(&piece_id).context("piece expired")?;
        buf.copy_from_slice(&inmp.bytes[piece_offset..(piece_offset + buf.len())]);
        Ok(())
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        let fi = &self.file_infos[file_id];
        let abs_offset = fi.offset_in_torrent + offset;
        let piece_id: u32 = (abs_offset / self.lengths.default_piece_length() as u64).try_into()?;
        let piece_offset: usize =
            (abs_offset % self.lengths.default_piece_length() as u64).try_into()?;
        let piece_id = self.lengths.validate_piece_index(piece_id).context("bug")?;
        let mut g = self.map.write();
        let inmp = g
            .entry(piece_id)
            .or_insert_with(|| InMemoryPiece::new(&self.lengths));
        inmp.bytes[piece_offset..(piece_offset + buf.len())].copy_from_slice(buf);
        Ok(())
    }

    fn remove_file(&self, _file_id: usize, _filename: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    fn ensure_file_length(&self, _file_id: usize, _length: u64) -> anyhow::Result<()> {
        Ok(())
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        let map = {
            let mut g = self.map.write();
            let mut repl = HashMap::new();
            std::mem::swap(&mut *g, &mut repl);
            repl
        };
        Ok(Box::new(Self {
            lengths: self.lengths,
            map: RwLock::new(map),
            file_infos: self.file_infos.clone(),
        }))
    }
}
