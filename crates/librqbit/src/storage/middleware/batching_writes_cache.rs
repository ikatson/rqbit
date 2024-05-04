/*
A storage middleware that caches pieces in memory, so that subsequent reads (for checksumming) are
free.

An example, untested and unproven to be useful.
*/

use anyhow::Context;
use librqbit_core::{
    constants::CHUNK_SIZE,
    lengths::{Lengths, ValidPieceIndex},
};
use tracing::{trace, warn};

use crate::{
    constants::MAX_LIVE_PEERS_PER_TORRENT,
    storage::{pwrite_all_absolute, StorageFactory, StorageFactoryExt, TorrentStorage},
    FileInfos,
};

#[derive(Clone, Copy)]
pub struct BatchingWritesCacheStorageFactory<U> {
    max_cache_bytes: u64,
    underlying: U,
}

impl<U> BatchingWritesCacheStorageFactory<U> {
    pub fn new(max_cache_bytes: u64, underlying: U) -> Self {
        Self {
            max_cache_bytes,
            underlying,
        }
    }
}

impl<U: StorageFactory + Clone> StorageFactory for BatchingWritesCacheStorageFactory<U> {
    type Storage = BatchingWritesCacheStorage<U::Storage>;

    fn init_storage(&self, info: &crate::ManagedTorrentInfo) -> anyhow::Result<Self::Storage> {
        let max_pieces = MAX_LIVE_PEERS_PER_TORRENT;
        let cache_bytes_per_piece: usize =
            (self.max_cache_bytes / max_pieces as u64 / CHUNK_SIZE as u64 * CHUNK_SIZE as u64)
                .try_into()
                .context("cache_bytes_per_piece")?;

        if cache_bytes_per_piece == 0 {
            const MIN_CACHE_BYTES: u64 = CHUNK_SIZE as u64 * MAX_LIVE_PEERS_PER_TORRENT as u64;
            anyhow::bail!(
                "min cache size is {}, but passed in {} is too low",
                MIN_CACHE_BYTES,
                self.max_cache_bytes
            );
        }
        Ok(BatchingWritesCacheStorage {
            max_pieces,
            cache_bytes_per_piece,
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
    start_offset: u32,
    len: u32,
    data: Box<[u8]>,
}

impl std::fmt::Debug for PieceCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PieceCache")
            .field("len", &self.len)
            .field("start_offset", &self.start_offset)
            .finish()
    }
}

enum AppendError {
    NonContiguous,
    NotEnoughSpace,
}

impl PieceCache {
    fn remaining(&self) -> usize {
        self.data.len() - self.len as usize
    }

    fn capacity(&self) -> usize {
        self.data.len()
    }

    fn filled(&self) -> &[u8] {
        &self.data[..self.len as usize]
    }

    fn replace_with(&mut self, piece_offset: u32, buf: &[u8]) {
        self.data[..buf.len()].copy_from_slice(buf);
        self.start_offset = piece_offset;
        self.len = buf.len().try_into().unwrap()
    }

    fn try_append(&mut self, piece_offset: u32, buf: &[u8]) -> Result<(), AppendError> {
        if self.len != 0 {
            if piece_offset != self.start_offset + self.len {
                return Err(AppendError::NonContiguous);
            }
        } else {
            self.start_offset = piece_offset;
        }

        if buf.len() > self.remaining() {
            return Err(AppendError::NotEnoughSpace);
        }
        self.data[self.len as usize..(self.len as usize + buf.len())].copy_from_slice(buf);
        self.len += TryInto::<u32>::try_into(buf.len()).unwrap();
        Ok(())
    }
}

pub struct BatchingWritesCacheStorage<U> {
    max_pieces: usize,
    cache_bytes_per_piece: usize,
    map: dashmap::DashMap<ValidPieceIndex, PieceCache>,
    lengths: Lengths,
    file_infos: FileInfos,
    underlying: U,
}

impl<U: TorrentStorage> BatchingWritesCacheStorage<U> {
    fn new_piece_cache(&self) -> PieceCache {
        PieceCache {
            start_offset: u32::MAX, // invalid value,
            len: 0,
            data: vec![0u8; self.cache_bytes_per_piece].into_boxed_slice(),
        }
    }

    fn flush(&self, piece_id: ValidPieceIndex, cache: &mut PieceCache) -> anyhow::Result<()> {
        trace!(
            piece_id = ?piece_id,
            piece_offset = cache.start_offset,
            cache_len = cache.len,
            "flushing"
        );
        let piece_offset = self.lengths.piece_offset(piece_id);
        let abs_offset = piece_offset + cache.start_offset as u64;
        pwrite_all_absolute(
            &self.underlying,
            abs_offset,
            cache.filled(),
            &self.file_infos,
        )?;
        cache.start_offset = u32::MAX; // invalid value
        cache.len = 0;
        Ok(())
    }
}

fn intervals_ranges(
    left: std::ops::Range<u64>,
    right: std::ops::Range<u64>,
) -> [(bool, std::ops::Range<u64>); 3] {
    let mut res = [(true, 0..0), (true, 0..0), (true, 0..0)];

    // beginning
    let start = left.start.min(right.start);
    let end = left.start.max(right.start);
    if start != end {
        let is_left = start == left.start;
        res[0] = (is_left, start..end)
    }

    // intersection
    let start = end;
    let end = left.end.min(right.end);
    if start != end {
        let is_left = start == left.start;
        res[0] = (is_left, start..end)
    }
    // end

    res
}

impl<U: TorrentStorage> TorrentStorage for BatchingWritesCacheStorage<U> {
    fn pread_exact(&self, file_id: usize, offset: u64, mut buf: &mut [u8]) -> anyhow::Result<()> {
        // try to read from cache first.
        // TODO: not sure if it's any faster than reading from disk.
        let cp = self
            .lengths
            .compute_current_piece(offset, self.file_infos[file_id].offset_in_torrent)
            .context("pread_exact: compute_current_piece returned None")?;

        if let Some(r) = self.map.get(&cp.id) {
            let pc = r.value();

            if pc.len > 0 {
                let pc_abs_offset = self.lengths.piece_offset(cp.id) + pc.start_offset as u64;
                let pc_abs_end = pc_abs_offset + pc.len as u64;
                let mut current_offset = offset;
                while !buf.is_empty() {
                    let mut to_read = 0;
                    match current_offset.cmp(&pc_abs_offset) {
                        std::cmp::Ordering::Less => {
                            // Read from disk up to pc_abs_offset
                            to_read =
                                (buf.len() as u64).min(pc_abs_offset - current_offset) as usize;
                            self.underlying.pread_exact(
                                file_id,
                                current_offset,
                                &mut buf[..to_read],
                            )?;
                        }
                        std::cmp::Ordering::Equal => {
                            to_read = (pc.len as usize).min(buf.len());
                            trace!(current_offset, to_read, skip=0, pc=?*pc, cp=?cp, "reading from cache");
                            buf[..to_read].copy_from_slice(&pc.data[..to_read]);
                        }
                        std::cmp::Ordering::Greater => {
                            // inside or outside of the cache
                            if current_offset < pc_abs_end {
                                to_read =
                                    (pc_abs_end - current_offset).min(buf.len() as u64) as usize;
                                let skip = (current_offset - pc_abs_offset) as usize;
                                trace!(current_offset, to_read, skip, pc=?*pc, cp=?cp, "reading from cache");
                                buf[..to_read].copy_from_slice(&pc.data[skip..skip + to_read]);
                            } else {
                                // read the remainder of the file from underlying storage
                                return self.underlying.pread_exact(file_id, current_offset, buf);
                            }
                        }
                    }

                    buf = &mut buf[to_read..];
                    current_offset += to_read as u64;
                }

                return Ok(());
            }
        }
        self.underlying.pread_exact(file_id, offset, buf)
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        let cp = self
            .lengths
            .compute_current_piece(offset, self.file_infos[file_id].offset_in_torrent)
            .context("pwrite_all: compute_current_piece returned None")?;

        // If the cache is too big, passthrough and warn.
        // This shouldn't happen.
        //
        // If the newly written chunk for the piece isn't adjacent, flush and replace.
        //
        // If the newly written chunk doesn't fit, flush and replace.
        // - if doens't FULLY fit, warn

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
        if let Err(e) = pc.try_append(cp.piece_offset, buf) {
            match e {
                AppendError::NonContiguous => {
                    trace!(cp = ?cp, len=buf.len(), pc=?*pc, file_id, offset, "non contiguous append, flushing")
                }
                AppendError::NotEnoughSpace => {
                    trace!(cp = ?cp, len=buf.len(), pc=?*pc, file_id, offset, "not enough space, flushing")
                }
            }

            self.flush(cp.id, &mut pc)?;

            if pc.capacity() >= buf.len() {
                pc.replace_with(cp.piece_offset, buf);
                Ok(())
            } else {
                self.underlying.pwrite_all(file_id, offset, buf)
            }
        } else {
            trace!(cp = ?cp, len=buf.len(), pc=?*pc, file_id, offset, "appended!");
            Ok(())
        }
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
        let new = BatchingWritesCacheStorage {
            max_pieces: self.max_pieces,
            cache_bytes_per_piece: self.cache_bytes_per_piece,
            map: Default::default(),
            lengths: self.lengths,
            file_infos: self.file_infos.clone(),
            underlying: self.underlying.take()?,
        };
        Ok(Box::new(new))
    }
}
