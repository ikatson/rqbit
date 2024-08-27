use std::{collections::HashMap, path::Path};

use anyhow::Context;
use librqbit_core::lengths::{Lengths, ValidPieceIndex};
use parking_lot::RwLock;

use crate::type_aliases::FileInfos;

use crate::storage::{StorageFactory, StorageFactoryExt, TorrentStorage};

pub struct InMemoryPiece {
    pub content: Box<[u8]>,
    pub has_been_validated: bool,
}

impl InMemoryPiece {
    pub fn new(l: &Lengths) -> Self {
        let v = vec![0; l.default_piece_length() as usize].into_boxed_slice();
        Self {
            content: v,
            has_been_validated: false,
        }
    }

    pub fn can_be_discard(&self, upper_bound_offset: usize) -> bool {
        self.has_been_validated && upper_bound_offset >= self.content.len()
    }
}

#[derive(Default, Clone)]
pub struct InMemoryExampleStorageFactory {
    max_ram_size_per_torrent: usize,
}

impl InMemoryExampleStorageFactory {
    pub fn new(max_ram_size_per_torrent: usize) -> Self {
        Self {
            max_ram_size_per_torrent,
        }
    }
}

impl StorageFactory for InMemoryExampleStorageFactory {
    type Storage = InMemoryExampleStorage;

    fn create(
        &self,
        info: &crate::torrent_state::ManagedTorrentShared,
    ) -> anyhow::Result<InMemoryExampleStorage> {
        InMemoryExampleStorage::new(
            info.lengths,
            info.file_infos.clone(),
            self.max_ram_size_per_torrent,
        )
    }

    fn clone_box(&self) -> crate::storage::BoxStorageFactory {
        self.clone().boxed()
    }
}

pub struct InMemoryExampleStorage {
    lengths: Lengths,
    file_infos: FileInfos,
    map: RwLock<HashMap<ValidPieceIndex, InMemoryPiece>>,
    max_ram_size_per_torrent: usize,
}

impl InMemoryExampleStorage {
    fn new(
        lengths: Lengths,
        file_infos: FileInfos,
        max_ram_size_per_torrent: usize,
    ) -> anyhow::Result<Self> {
        // Max memory 128MiB. Make it tunable
        let max_pieces = 128 * 1024 * 1024 / lengths.default_piece_length();
        if max_pieces == 0 {
            anyhow::bail!("pieces too large");
        }

        Ok(Self {
            lengths,
            file_infos,
            map: RwLock::new(HashMap::new()),
            max_ram_size_per_torrent,
        })
    }
}

impl TorrentStorage for InMemoryExampleStorage {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        let fi = &self.file_infos[file_id];
        let abs_offset = fi.offset_in_torrent + offset;
        let piece_id: u32 = (abs_offset / self.lengths.default_piece_length() as u64).try_into()?;
        let piece_offset: usize =
            (abs_offset % self.lengths.default_piece_length() as u64).try_into()?;
        let piece_id = self.lengths.validate_piece_index(piece_id).context("bug")?;

        let mut g = self.map.write();
        // Get and remove this data from buffer to free space
        let inmp = g.get(&piece_id).context("piece expired")?;
        let upper_bound_offset = piece_offset + buf.len();
        buf.copy_from_slice(&inmp.content[piece_offset..upper_bound_offset]);

        if inmp.can_be_discard(upper_bound_offset) {
            let _ = g.remove(&piece_id);
        }

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
        inmp.content[piece_offset..(piece_offset + buf.len())].copy_from_slice(buf);
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
            max_ram_size_per_torrent: self.max_ram_size_per_torrent,
        }))
    }

    fn init(&mut self, _meta: &crate::ManagedTorrentShared) -> anyhow::Result<()> {
        Ok(())
    }

    fn remove_directory_if_empty(&self, _path: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_piece_completed(&self, file_id: usize, offset: u64) -> anyhow::Result<()> {
        let fi = &self.file_infos[file_id];
        let abs_offset = fi.offset_in_torrent + offset;
        let piece_id: u32 = (abs_offset / self.lengths.default_piece_length() as u64).try_into()?;
        let piece_id = self.lengths.validate_piece_index(piece_id).context("bug")?;

        let mut g = self.map.write();
        let inmp = g.get_mut(&piece_id).context("piece does not exist")?;

        inmp.has_been_validated = true;

        Ok(())
    }
}
