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
    storage::{
        MoveStorageOutcome, MoveStorageRequest, StorageFactory, StorageFactoryExt, TorrentStorage,
    },
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
            lengths: self.lengths.clone(),
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

    fn on_piece_completed(&self, piece_index: ValidPieceIndex) -> anyhow::Result<()> {
        self.underlying.on_piece_completed(piece_index)
    }

    fn file_length(&self, file_id: usize) -> anyhow::Result<Option<u64>> {
        self.underlying.file_length(file_id)
    }

    fn rename_file(
        &self,
        file_id: usize,
        from: &std::path::Path,
        to: &std::path::Path,
    ) -> anyhow::Result<()> {
        // The bytes don't change, only where they live, so the cache stays valid.
        self.underlying.rename_file(file_id, from, to)
    }

    fn move_storage(&self, request: &MoveStorageRequest<'_>) -> anyhow::Result<MoveStorageOutcome> {
        let outcome = self.underlying.move_storage(request)?;
        // A file adopted at the destination holds other bytes than the ones cached here,
        // and the recheck that follows must read those.
        if !outcome.adopted_files.is_empty() {
            self.lru.write().clear();
        }
        Ok(outcome)
    }

    fn finish_move_storage(&self, commit: bool) -> anyhow::Result<()> {
        self.underlying.finish_move_storage(commit)
    }

    fn forget_cached_data(&self) -> anyhow::Result<()> {
        // A check verifies what is stored: a piece damaged on disk is still right here.
        self.lru.write().clear();
        self.underlying.forget_cached_data()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_util::{
        NUM_PIECES, PIECE_LEN, Probe, assert_forwards_defaults, lengths,
    };

    const TOTAL_LEN: usize = PIECE_LEN as usize * NUM_PIECES as usize;

    // A cache with room for every piece, over a single file of the test lengths.
    fn cache_over<U>(underlying: U) -> WriteThroughCacheStorage<U> {
        WriteThroughCacheStorage {
            lru: RwLock::new(LruCache::new(
                NonZeroUsize::new(NUM_PIECES as usize).unwrap(),
            )),
            lengths: lengths(),
            file_infos: vec![crate::file_info::FileInfo {
                relative_filename: "test.dat".into(),
                offset_in_torrent: 0,
                len: TOTAL_LEN as u64,
                piece_range: 0..NUM_PIECES,
                attrs: Default::default(),
            }],
            underlying,
        }
    }

    // The single file under the cache, in memory, so that a test can change it behind the
    // cache's back like a disk that gets damaged.
    struct Disk {
        bytes: parking_lot::Mutex<Vec<u8>>,
    }

    impl TorrentStorage for Disk {
        fn init(
            &mut self,
            _shared: &ManagedTorrentShared,
            _metadata: &TorrentMetadata,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        fn pread_exact(&self, _file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
            let start = usize::try_from(offset)?;
            let g = self.bytes.lock();
            let src = g
                .get(start..start + buf.len())
                .context("read past the end")?;
            buf.copy_from_slice(src);
            Ok(())
        }

        fn pwrite_all(&self, _file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
            let start = usize::try_from(offset)?;
            let mut g = self.bytes.lock();
            g.get_mut(start..start + buf.len())
                .context("write past the end")?
                .copy_from_slice(buf);
            Ok(())
        }

        fn remove_file(&self, _file_id: usize, _filename: &std::path::Path) -> anyhow::Result<()> {
            Ok(())
        }

        fn remove_directory_if_empty(&self, _path: &std::path::Path) -> anyhow::Result<()> {
            Ok(())
        }

        fn ensure_file_length(&self, _file_id: usize, _length: u64) -> anyhow::Result<()> {
            Ok(())
        }

        fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
            anyhow::bail!("not used")
        }
    }

    #[test]
    fn test_the_defaulted_methods_reach_the_underlying_storage() {
        let storage = cache_over(Probe::default());
        assert_forwards_defaults(&storage, &storage.underlying);
    }

    // A written piece is answered from the cache, even once the disk holds other bytes.
    // After forget_cached_data() reads see the disk again, which is what a recheck needs.
    #[test]
    fn test_forget_cached_data_makes_reads_see_the_disk() {
        let piece_len = PIECE_LEN as usize;
        let storage = cache_over(Disk {
            bytes: parking_lot::Mutex::new(vec![0; TOTAL_LEN]),
        });
        storage.pwrite_all(0, 0, &vec![1; piece_len]).unwrap();
        storage.underlying.bytes.lock()[..piece_len].fill(2);

        let mut buf = vec![0; piece_len];
        storage.pread_exact(0, 0, &mut buf).unwrap();
        assert!(
            buf.iter().all(|b| *b == 1),
            "the piece wasn't read from the cache"
        );

        storage.forget_cached_data().unwrap();
        storage.pread_exact(0, 0, &mut buf).unwrap();
        assert!(
            buf.iter().all(|b| *b == 2),
            "the piece was still read from the cache after forget_cached_data()"
        );
    }
}
