/*
A storage middleware that caches pieces in memory, so that subsequent reads (for checksumming) are
free.

An example, untested and unproven to be useful.
*/

use std::num::NonZeroUsize;

use anyhow::Context;
use librqbit_core::lengths::{Lengths, ValidPieceIndex};
use lru::LruCache;
use parking_lot::RwLock;

use crate::{
    storage::{StorageFactory, StorageFactoryExt, TorrentStorage},
    FileInfos,
};

#[derive(Clone, Copy)]
pub struct WriteThroughCacheStorageFactory<U> {
    max_cache_bytes: u64,
    underlying: U,
}

impl<U> WriteThroughCacheStorageFactory<U> {
    pub fn new(max_cache_bytes: u64, underlying: U) -> Self {
        Self {
            max_cache_bytes,
            underlying,
        }
    }
}

impl<U: StorageFactory + Clone> StorageFactory for WriteThroughCacheStorageFactory<U> {
    type Storage = WriteThroughCacheStorage<U::Storage>;

    fn init_storage(&self, info: &crate::ManagedTorrentInfo) -> anyhow::Result<Self::Storage> {
        let pieces = self
            .max_cache_bytes
            .div_ceil(info.lengths.default_piece_length() as u64)
            .try_into()?;
        let pieces = NonZeroUsize::new(pieces).context("bug: pieces == 0")?;
        let lru = RwLock::new(LruCache::new(pieces));
        Ok(WriteThroughCacheStorage {
            lru,
            underlying: self.underlying.init_storage(info)?,
            lengths: info.lengths,
            file_infos: info.file_infos.clone(),
        })
    }

    fn clone_box(&self) -> crate::storage::BoxStorageFactory {
        self.clone().boxed()
    }
}

pub struct WriteThroughCacheStorage<U> {
    lru: RwLock<LruCache<ValidPieceIndex, Box<[u8]>>>,
    lengths: Lengths,
    file_infos: FileInfos,
    underlying: U,
}

impl<U: TorrentStorage> TorrentStorage for WriteThroughCacheStorage<U> {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        let file = self.file_infos.get(file_id).context("wrong file")?;
        let current = self
            .lengths
            .compute_current_piece(offset, file.offset_in_torrent)
            .context("wrong piece")?;
        let mut g = self.lru.write();
        if let Some(p) = g.get(&current.id) {
            let start = current.piece_offset as usize;
            let end = start + buf.len();
            let pbuf = p.get(start..end).context("bugged length")?;
            buf.copy_from_slice(pbuf);
            return Ok(());
        }
        self.underlying.pread_exact(file_id, offset, buf)
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        let file = self.file_infos.get(file_id).context("wrong file")?;
        let current = self
            .lengths
            .compute_current_piece(offset, file.offset_in_torrent)
            .context("wrong piece")?;
        let mut g = self.lru.write();
        let pbuf = g.get_or_insert_mut(current.id, || {
            vec![0; self.lengths.piece_length(current.id) as usize].into_boxed_slice()
        });
        let start = current.piece_offset as usize;
        let end = start + buf.len();
        pbuf.get_mut(start..end)
            .context("bugged range")?
            .copy_from_slice(buf);
        self.underlying.pwrite_all(file_id, offset, buf)
    }

    fn remove_file(&self, file_id: usize, filename: &std::path::Path) -> anyhow::Result<()> {
        self.underlying.remove_file(file_id, filename)
    }

    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()> {
        self.underlying.ensure_file_length(file_id, length)
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        let replacement_cache = LruCache::new(NonZeroUsize::new(1).context("unreachable")?);
        let lru = std::mem::replace(&mut *self.lru.write(), replacement_cache);
        Ok(Box::new(WriteThroughCacheStorage {
            lru: RwLock::new(lru),
            underlying: self.underlying.take()?,
            lengths: self.lengths,
            file_infos: self.file_infos.clone(),
        }))
    }
}
