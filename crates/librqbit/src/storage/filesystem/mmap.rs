use std::path::Path;

use anyhow::Context;
use memmap2::{MmapMut, MmapOptions};
use parking_lot::RwLock;

use crate::torrent_state::{ManagedTorrentShared, TorrentMetadata};

use crate::storage::{StorageFactory, StorageFactoryExt, TorrentStorage};

use super::{FilesystemStorage, FilesystemStorageFactory};

#[derive(Default, Clone, Copy)]
pub struct MmapFilesystemStorageFactory {}

type OpenedMmap = RwLock<MmapMut>;

fn dummy_mmap() -> anyhow::Result<MmapMut> {
    Ok(memmap2::MmapOptions::new().len(1).map_anon()?)
}

impl StorageFactory for MmapFilesystemStorageFactory {
    type Storage = MmapFilesystemStorage;

    fn create(
        &self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<Self::Storage> {
        let fs_storage = FilesystemStorageFactory::default().create(shared, metadata)?;

        Ok(MmapFilesystemStorage {
            opened_mmaps: Vec::new(),
            fs: fs_storage,
        })
    }

    fn clone_box(&self) -> crate::storage::BoxStorageFactory {
        self.boxed()
    }
}

pub struct MmapFilesystemStorage {
    opened_mmaps: Vec<OpenedMmap>,
    fs: FilesystemStorage,
}

impl TorrentStorage for MmapFilesystemStorage {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        let g = self
            .opened_mmaps
            .get(file_id)
            .context("no such file")?
            .read();
        let start = offset;
        let end = offset + buf.len() as u64;
        let start = start.try_into()?;
        let end = end.try_into()?;
        buf.copy_from_slice(g.get(start..end).context("bug")?);
        Ok(())
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        let mut g = self
            .opened_mmaps
            .get(file_id)
            .context("no such file")?
            .write();
        let start = offset;
        let end = offset + buf.len() as u64;
        let start = start.try_into()?;
        let end = end.try_into()?;
        g.get_mut(start..end).context("bug")?.copy_from_slice(buf);
        Ok(())
    }

    fn remove_file(&self, file_id: usize, filename: &Path) -> anyhow::Result<()> {
        self.fs.remove_file(file_id, filename)
    }

    fn remove_directory_if_empty(&self, path: &Path) -> anyhow::Result<()> {
        self.fs.remove_directory_if_empty(path)
    }

    fn ensure_file_length(&self, file_id: usize, len: u64) -> anyhow::Result<()> {
        self.fs.ensure_file_length(file_id, len)
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        Ok(Box::new(Self {
            opened_mmaps: self
                .opened_mmaps
                .iter()
                .map(|m| {
                    let d = dummy_mmap()?;
                    let mut g = m.write();
                    Ok::<_, anyhow::Error>(RwLock::new(std::mem::replace(&mut *g, d)))
                })
                .collect::<anyhow::Result<_>>()?,
            fs: self.fs.take_fs()?,
        }))
    }

    fn init(
        &mut self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        self.fs.init(shared, metadata)?;
        let mut mmaps = Vec::new();
        for (idx, fi) in metadata.file_infos.iter().enumerate() {
            if fi.attrs.padding {
                mmaps.push(RwLock::new(dummy_mmap()?));
                continue;
            }
            let file = self.fs.get_or_open(idx, true)?;
            file.set_len(fi.len)
                .context("mmap storage: error setting length")?;
            let mmap =
                unsafe { MmapOptions::new().map_mut(&*file) }.context("error mapping file")?;
            mmaps.push(RwLock::new(mmap));
        }

        self.opened_mmaps = mmaps;
        Ok(())
    }
}
