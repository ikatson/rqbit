use anyhow::Context;
use memmap2::{MmapMut, MmapOptions};
use parking_lot::RwLock;

use crate::{
    storage::{StorageFactory, StorageFactoryExt, TorrentStorage},
    FileInfos, ManagedTorrentInfo,
};

#[derive(Default, Clone)]
pub struct MmapStorageFactory {}

pub struct MmapStorage {
    mmap: RwLock<MmapMut>,
    file_infos: FileInfos,
}

impl StorageFactory for MmapStorageFactory {
    type Storage = MmapStorage;

    fn init_storage(&self, info: &ManagedTorrentInfo) -> anyhow::Result<Self::Storage> {
        Ok(MmapStorage {
            mmap: RwLock::new(
                MmapOptions::new()
                    .len(info.lengths.total_length().try_into()?)
                    .map_anon()?,
            ),
            file_infos: info.file_infos.clone(),
        })
    }

    fn clone_box(&self) -> crate::storage::BoxStorageFactory {
        self.clone().boxed()
    }
}

impl TorrentStorage for MmapStorage {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        let start: usize = (self.file_infos[file_id].offset_in_torrent + offset).try_into()?;
        let end = start + buf.len();
        buf.copy_from_slice(self.mmap.read().get(start..end).context("bad range")?);
        Ok(())
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        let start: usize = (self.file_infos[file_id].offset_in_torrent + offset).try_into()?;
        let end = start + buf.len();
        let mut g = self.mmap.write();
        let target = g.get_mut(start..end).context("bad range")?;
        target.copy_from_slice(buf);
        Ok(())
    }

    fn remove_file(&self, _file_id: usize, _filename: &std::path::Path) -> anyhow::Result<()> {
        Ok(())
    }

    fn ensure_file_length(&self, _file_id: usize, _length: u64) -> anyhow::Result<()> {
        Ok(())
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        anyhow::bail!("not implemented")
    }
}
