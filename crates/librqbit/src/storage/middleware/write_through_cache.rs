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
    FileInfos, ManagedTorrentShared,
    storage::{StorageFactory, StorageFactoryExt, TorrentStorage},
    torrent_state::TorrentMetadata,
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

    fn create(
        &self,
        shared: &crate::ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<Self::Storage> {
        let pieces = self
            .max_cache_bytes
            .div_ceil(metadata.info.lengths().default_piece_length() as u64)
            .try_into()?;
        let pieces = NonZeroUsize::new(pieces).context("bug: pieces == 0")?;
        let lru = RwLock::new(LruCache::new(pieces));
        Ok(WriteThroughCacheStorage {
            lru,
            underlying: self.underlying.create(shared, metadata)?,
            lengths: *metadata.info.lengths(),
            file_infos: metadata.file_infos.clone(),
        })
    }

    fn is_type_id(&self, type_id: std::any::TypeId) -> bool {
        self.underlying.is_type_id(type_id)
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

impl<U> WriteThroughCacheStorage<U> {
    // Put the bytes in the cache. Handing them on to the storage underneath is the
    // caller's job: both write paths cache the same way and differ only in how they write.
    //
    // The bufs are one write: consecutive from offset, and the second half is empty for a
    // plain pwrite_all(). A write covers one piece, so both halves go in under one lock
    // and one LRU lookup - taking them one at a time would cost two of each for a write
    // the storage does once.
    fn cache(&self, file_id: usize, offset: u64, bufs: [&[u8]; 2]) -> anyhow::Result<()> {
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
        let end = start + bufs[0].len() + bufs[1].len();
        let dest = pbuf.get_mut(start..end).context("bugged range")?;
        let (first, second) = dest.split_at_mut(bufs[0].len());
        first.copy_from_slice(bufs[0]);
        second.copy_from_slice(bufs[1]);
        Ok(())
    }
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
        self.cache(file_id, offset, [buf, &[]])?;
        self.underlying.pwrite_all(file_id, offset, buf)
    }

    fn pwrite_all_vectored(
        &self,
        file_id: usize,
        offset: u64,
        bufs: [std::io::IoSlice<'_>; 2],
    ) -> anyhow::Result<usize> {
        // Cache both halves, then hand the write on as one call. The default would turn it
        // into two pwrite_all()s, and the storage underneath would lose the single
        // positioned vectored write it is written to do.
        self.cache(file_id, offset, [&bufs[0], &bufs[1]])?;
        self.underlying.pwrite_all_vectored(file_id, offset, bufs)
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

    fn remove_directory_if_empty(&self, path: &std::path::Path) -> anyhow::Result<()> {
        self.underlying.remove_directory_if_empty(path)
    }

    fn init(
        &mut self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        self.underlying.init(shared, metadata)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_util::{Probe, assert_forwards_vectored_writes};

    const PIECE_LEN: u32 = librqbit_core::constants::CHUNK_SIZE * 2;
    const NUM_PIECES: u32 = 2;

    #[test]
    fn test_a_vectored_write_reaches_the_underlying_storage() {
        let storage = WriteThroughCacheStorage {
            lru: RwLock::new(LruCache::new(NonZeroUsize::new(1).unwrap())),
            lengths: Lengths::new(PIECE_LEN as u64 * NUM_PIECES as u64, PIECE_LEN).unwrap(),
            file_infos: vec![crate::file_info::FileInfo {
                relative_filename: "test.dat".into(),
                offset_in_torrent: 0,
                len: PIECE_LEN as u64 * NUM_PIECES as u64,
                piece_range: 0..NUM_PIECES,
                attrs: Default::default(),
            }],
            underlying: Probe::default(),
        };
        assert_forwards_vectored_writes(&storage, &storage.underlying);
    }
}
