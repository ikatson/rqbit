/*
A storage middleware that caches pieces in memory, and only flushes them if they are successfully
checksummed.

Requires a lot of memory, unproven to be useful. The intention is to issue less random writes to disk.
*/

use anyhow::Context;
use librqbit_core::lengths::{Lengths, ValidPieceIndex};
use tracing::{trace, warn};

use crate::{
    constants::MAX_LIVE_PEERS_PER_TORRENT,
    storage::{pwrite_all_absolute, StorageFactory, StorageFactoryExt, TorrentStorage},
    FileInfos,
};

#[derive(Clone, Copy)]
pub struct FullPieceCacheStorageFactory<U> {
    max_cache_bytes: u64,
    underlying: U,
}

impl<U> FullPieceCacheStorageFactory<U> {
    pub fn new(max_cache_bytes: u64, underlying: U) -> Self {
        Self {
            max_cache_bytes,
            underlying,
        }
    }
}

impl<U: StorageFactory + Clone> StorageFactory for FullPieceCacheStorageFactory<U> {
    type Storage = FullPieceCacheStorage<U::Storage>;

    fn init_storage(&self, info: &crate::ManagedTorrentInfo) -> anyhow::Result<Self::Storage> {
        let max_pieces = MAX_LIVE_PEERS_PER_TORRENT;
        let required_memory = info.lengths.default_piece_length() as u64 * max_pieces as u64;
        if required_memory > self.max_cache_bytes {
            const MB: u64 = 1024 * 1024;
            anyhow::bail!(
                "not enough memory to init FullPieceCacheStorageFactory. Required {} mb, allowed only {}",
                required_memory.div_ceil(MB),
                self.max_cache_bytes.div_ceil(MB)
            )
        }
        Ok(FullPieceCacheStorage {
            max_pieces,
            map: Default::default(),
            lengths: info.lengths,
            file_infos: info.file_infos.clone(),
            underlying: self.underlying.init_storage(info)?,
        })
    }

    fn clone_box(&self) -> crate::storage::BoxStorageFactory {
        self.clone().boxed()
    }
}

struct PieceCache {
    data: Box<[u8]>,
}

impl std::fmt::Debug for PieceCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PieceCache").finish()
    }
}

pub struct FullPieceCacheStorage<U> {
    max_pieces: usize,
    map: dashmap::DashMap<ValidPieceIndex, PieceCache>,
    lengths: Lengths,
    file_infos: FileInfos,
    underlying: U,
}

impl<U: TorrentStorage> FullPieceCacheStorage<U> {
    fn new_piece_cache(&self) -> PieceCache {
        PieceCache {
            data: vec![0u8; self.lengths.default_piece_length() as usize].into_boxed_slice(),
        }
    }

    fn flush(&self, piece_id: ValidPieceIndex, cache: &mut PieceCache) -> anyhow::Result<()> {
        let piece_offset = self.lengths.piece_offset(piece_id);
        let len = self.lengths.piece_length(piece_id);
        trace!(?piece_id, piece_offset, len, "flushing");
        pwrite_all_absolute(
            &self.underlying,
            piece_offset,
            &cache.data[..len as usize],
            &self.file_infos,
        )?;
        Ok(())
    }
}

impl<U: TorrentStorage> TorrentStorage for FullPieceCacheStorage<U> {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        let cp = self
            .lengths
            .compute_current_piece(offset, self.file_infos[file_id].offset_in_torrent)
            .context("pread_exact: compute_current_piece returned None")?;

        if let Some(r) = self.map.get(&cp.id) {
            let pc = r.value();
            buf.copy_from_slice(
                &pc.data[cp.piece_offset as usize..cp.piece_offset as usize + buf.len()],
            );
            Ok(())
        } else {
            self.underlying.pread_exact(file_id, offset, buf)
        }
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        let cp = self
            .lengths
            .compute_current_piece(offset, self.file_infos[file_id].offset_in_torrent)
            .context("pwrite_all: compute_current_piece returned None")?;

        use dashmap::mapref::entry::Entry;
        let clen = self.map.len();
        let mut pc = match self.map.entry(cp.id) {
            Entry::Occupied(occ) => occ.into_ref(),
            Entry::Vacant(vac) => {
                if clen >= self.max_pieces {
                    warn!(
                        "map len = {}, expected it to be <= {} without triggering this warning",
                        clen, self.max_pieces
                    );
                    return self.underlying.pwrite_all(file_id, offset, buf);
                }
                vac.insert(self.new_piece_cache())
            }
        };
        pc.data[cp.piece_offset as usize..cp.piece_offset as usize + buf.len()]
            .copy_from_slice(buf);
        Ok(())
    }

    fn remove_file(&self, file_id: usize, filename: &std::path::Path) -> anyhow::Result<()> {
        self.underlying.remove_file(file_id, filename)
    }

    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()> {
        self.underlying.ensure_file_length(file_id, length)
    }

    fn flush_piece(&self, piece_id: ValidPieceIndex) -> anyhow::Result<()> {
        if let Some((_, mut v)) = self.map.remove(&piece_id) {
            self.flush(piece_id, &mut v)?;
        } else {
            trace!(?piece_id, "no piece in cache, can't flush");
        }
        Ok(())
    }

    fn discard_piece(&self, piece_id: ValidPieceIndex) -> anyhow::Result<()> {
        if let Some((_, _v)) = self.map.remove(&piece_id) {
            trace!(?piece_id, "discarded");
        }
        Ok(())
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        for mut piece in self.map.iter_mut() {
            self.flush(*piece.key(), piece.value_mut())?;
        }
        let new = FullPieceCacheStorage {
            max_pieces: self.max_pieces,
            map: Default::default(),
            lengths: self.lengths,
            file_infos: self.file_infos.clone(),
            underlying: self.underlying.take()?,
        };
        Ok(Box::new(new))
    }
}
