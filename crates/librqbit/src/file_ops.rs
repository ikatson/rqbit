use std::{
    marker::PhantomData,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use anyhow::{Context, bail};
use buffers::{ByteBuf, ByteBufOwned};
use librqbit_core::{
    lengths::{ChunkInfo, ValidPieceIndex},
    torrent_metainfo::ValidatedTorrentMetaV1Info,
};
use peer_binary_protocol::{DoubleBufHelper, Piece};
use sha1w::{ISha1, Sha1};
use tracing::{debug, trace, warn};

use crate::{
    file_info::FileInfo,
    storage::TorrentStorage,
    type_aliases::{BF, FileInfos, PeerHandle},
};

pub fn update_hash_from_file<Sha1: ISha1>(
    file_id: usize,
    file_info: &FileInfo,
    mut pos: u64,
    files: &dyn TorrentStorage,
    hash: &mut Sha1,
    buf: &mut [u8],
    mut bytes_to_read: usize,
) -> anyhow::Result<()> {
    let mut read = 0;
    while bytes_to_read > 0 {
        let chunk = std::cmp::min(buf.len(), bytes_to_read);
        if file_info.attrs.padding {
            buf[..chunk].fill(0);
        } else {
            files
                .pread_exact(file_id, pos, &mut buf[..chunk])
                .with_context(|| {
                    format!("failed reading chunk of size {chunk}, read so far {read}")
                })?;
        }
        bytes_to_read -= chunk;
        read += chunk;
        pos += chunk as u64;
        hash.update(&buf[..chunk]);
    }
    Ok(())
}

pub(crate) struct FileOps<'a> {
    torrent: &'a ValidatedTorrentMetaV1Info<ByteBufOwned>,
    files: &'a dyn TorrentStorage,
    file_infos: &'a FileInfos,
    phantom_data: PhantomData<Sha1>,
}

impl<'a> FileOps<'a> {
    pub fn new(
        torrent: &'a ValidatedTorrentMetaV1Info<ByteBufOwned>,
        files: &'a dyn TorrentStorage,
        file_infos: &'a FileInfos,
    ) -> Self {
        Self {
            torrent,
            files,
            file_infos,
            phantom_data: PhantomData,
        }
    }

    /// Find the first file that overlaps `absolute_offset` (or the first non-empty file at that
    /// boundary) using binary search on `offset_in_torrent + len`, returning `(file_idx, offset_in_file)`.
    fn find_starting_file(&self, absolute_offset: u64) -> (usize, u64) {
        let start_idx = self
            .file_infos
            .partition_point(|fi| fi.offset_in_torrent + fi.len <= absolute_offset);
        let offset_in_file = self
            .file_infos
            .get(start_idx)
            .map(|fi| absolute_offset - fi.offset_in_torrent)
            .unwrap_or(0);
        (start_idx, offset_in_file)
    }

    // Returns the bitvector with pieces we have.
    pub fn initial_check(
        &self,
        progress: &AtomicU64,
        pause_requested: &AtomicBool,
    ) -> anyhow::Result<BF> {
        let mut have_pieces =
            BF::from_boxed_slice(vec![0u8; self.torrent.lengths().piece_bitfield_bytes()].into());

        #[derive(Debug)]
        struct CurrentFile<'a> {
            index: usize,
            fi: &'a FileInfo,
            processed_bytes: u64,
            is_broken: bool,
        }
        impl CurrentFile<'_> {
            fn remaining(&self) -> u64 {
                self.fi.len - self.processed_bytes
            }
            fn mark_processed_bytes(&mut self, bytes: u64) {
                self.processed_bytes += bytes
            }
        }
        let mut file_iterator = self
            .file_infos
            .iter()
            .enumerate()
            .map(|(idx, fi)| CurrentFile {
                index: idx,
                fi,
                processed_bytes: 0,
                is_broken: false,
            });

        let mut current_file = file_iterator.next().context("empty input file list")?;

        let mut read_buffer = vec![0u8; 65536];

        for piece_info in self.torrent.lengths().iter_piece_infos() {
            if pause_requested.load(Ordering::Relaxed) {
                bail!("initial check paused");
            }

            let mut computed_hash = Sha1::new();
            let mut piece_remaining = piece_info.len as usize;
            let mut some_files_broken = false;
            progress.fetch_add(piece_info.len as u64, Ordering::Relaxed);

            while piece_remaining > 0 {
                let mut to_read_in_file: usize =
                    std::cmp::min(current_file.remaining(), piece_remaining as u64).try_into()?;

                // Keep changing the current file to next until we find a file that has greater than 0 length.
                while to_read_in_file == 0 {
                    current_file = file_iterator.next().context("broken torrent metadata")?;

                    to_read_in_file =
                        std::cmp::min(current_file.remaining(), piece_remaining as u64)
                            .try_into()?;
                }

                let pos = current_file.processed_bytes;
                piece_remaining -= to_read_in_file;
                current_file.mark_processed_bytes(to_read_in_file as u64);

                if current_file.is_broken || some_files_broken {
                    // Either this file is already known broken, or an earlier file in this piece
                    // failed to read so the piece hash cannot match anyway.
                    some_files_broken = true;
                    continue;
                }

                if let Err(err) = update_hash_from_file(
                    current_file.index,
                    current_file.fi,
                    pos,
                    self.files,
                    &mut computed_hash,
                    &mut read_buffer,
                    to_read_in_file,
                ) {
                    debug!(
                        "error reading from file {} ({:?}) at {}: {:#}",
                        current_file.index, current_file.fi.relative_filename, pos, &err
                    );
                    current_file.is_broken = true;
                    some_files_broken = true;
                }
            }

            if some_files_broken {
                trace!(
                    "piece {} had errors, marking as needed",
                    piece_info.piece_index
                );
                continue;
            }

            if self
                .torrent
                .info()
                .compare_hash(piece_info.piece_index.get(), computed_hash.finish())
                .context("bug: either torrent info broken or we have a bug - piece index invalid")?
            {
                have_pieces.set(piece_info.piece_index.get() as usize, true);
            }
        }

        Ok(have_pieces)
    }

    pub fn check_piece(&self, piece_index: ValidPieceIndex) -> anyhow::Result<bool> {
        if cfg!(feature = "_disable_disk_write_net_benchmark") {
            return Ok(true);
        }

        let mut h = Sha1::new();
        let piece_length = self.torrent.lengths().piece_length(piece_index);
        let absolute_offset = self.torrent.lengths().piece_offset(piece_index);
        let mut buf = vec![0u8; std::cmp::min(65536, piece_length as usize)];

        let mut piece_remaining_bytes = piece_length as usize;
        let (start_idx, mut offset_in_file) = self.find_starting_file(absolute_offset);

        for (rel_idx, fi) in self.file_infos[start_idx..].iter().enumerate() {
            let file_idx = start_idx + rel_idx;
            let file_len = fi.len;
            if file_len == 0 {
                continue;
            }
            let file_remaining_len = file_len - offset_in_file;

            let to_read_in_file: usize =
                std::cmp::min(file_remaining_len, piece_remaining_bytes as u64).try_into()?;
            trace!(
                "piece={}, file_idx={}, seeking to {}",
                piece_index, file_idx, offset_in_file,
            );
            update_hash_from_file(
                file_idx,
                fi,
                offset_in_file,
                self.files,
                &mut h,
                &mut buf,
                to_read_in_file,
            )
            .with_context(|| {
                format!(
                    "error reading {to_read_in_file} bytes, file_id: {file_idx} (\"{:?}\")",
                    fi.relative_filename
                )
            })?;

            piece_remaining_bytes -= to_read_in_file;

            if piece_remaining_bytes == 0 {
                break;
            }

            offset_in_file = 0;
        }

        match self
            .torrent
            .info()
            .compare_hash(piece_index.get(), h.finish())
        {
            Some(true) => {
                trace!("piece={} hash matches", piece_index);
                Ok(true)
            }
            Some(false) => {
                let piece_length = self.torrent.lengths().piece_length(piece_index);
                let absolute_offset = self.torrent.lengths().piece_offset(piece_index);
                warn!(
                    piece_length,
                    absolute_offset, "the piece={} hash does not match", piece_index
                );
                Ok(false)
            }
            None => {
                // this is probably a bug?
                warn!("compare_hash() did not find the piece");
                anyhow::bail!("compare_hash() did not find the piece");
            }
        }
    }

    pub fn read_chunk(
        &self,
        who_sent: PeerHandle,
        chunk_info: &ChunkInfo,
        result_buf: &mut [u8],
    ) -> anyhow::Result<()> {
        if result_buf.len() < chunk_info.size as usize {
            anyhow::bail!("read_chunk(): not enough capacity in the provided buffer")
        }
        let absolute_offset = self.torrent.lengths().chunk_absolute_offset(chunk_info);
        let mut buf = &mut result_buf[..chunk_info.size as usize];
        let (start_idx, mut offset_in_file) = self.find_starting_file(absolute_offset);

        for (rel_idx, file_info) in self.file_infos[start_idx..].iter().enumerate() {
            let file_idx = start_idx + rel_idx;
            let file_len = file_info.len;
            if file_len == 0 {
                continue;
            }
            let file_remaining_len = file_len - offset_in_file;
            let to_read_in_file = std::cmp::min(file_remaining_len, buf.len() as u64).try_into()?;

            trace!(
                "piece={}, handle={}, file_idx={}, seeking to {}. To read chunk: {:?}",
                chunk_info.piece_index, who_sent, file_idx, offset_in_file, &chunk_info
            );
            if file_info.attrs.padding {
                buf[..to_read_in_file].fill(0);
            } else {
                self.files
                    .pread_exact(file_idx, offset_in_file, &mut buf[..to_read_in_file])
                    .with_context(|| {
                        format!("error reading {file_idx} bytes, file_id: {to_read_in_file}")
                    })?;
            }

            buf = &mut buf[to_read_in_file..];

            if buf.is_empty() {
                break;
            }

            offset_in_file = 0;
        }

        Ok(())
    }

    pub fn write_chunk(
        &self,
        who_sent: PeerHandle,
        data: &Piece<ByteBuf<'a>>,
        chunk_info: &ChunkInfo,
    ) -> anyhow::Result<()> {
        let absolute_offset = self.torrent.lengths().chunk_absolute_offset(chunk_info);
        let mut data = DoubleBufHelper::new(data.data().0, data.data().1);
        let (start_idx, mut offset_in_file) = self.find_starting_file(absolute_offset);

        for (rel_idx, file_info) in self.file_infos[start_idx..].iter().enumerate() {
            let file_idx = start_idx + rel_idx;
            let file_len = file_info.len;
            if file_len == 0 {
                continue;
            }

            let remaining_len = file_len - offset_in_file;
            let to_write = std::cmp::min(data.len() as u64, remaining_len).try_into()?;

            trace!(
                "piece={}, chunk={:?}, handle={}, begin={}, file={}, writing {} bytes at {}",
                chunk_info.piece_index,
                chunk_info,
                who_sent,
                chunk_info.offset,
                file_idx,
                to_write,
                offset_in_file
            );
            let slices = data.as_ioslices(to_write);
            debug_assert_eq!(slices[0].len() + slices[1].len(), to_write);
            if !file_info.attrs.padding {
                let written = self
                    .files
                    .pwrite_all_vectored(file_idx, offset_in_file, slices)
                    .with_context(|| {
                        format!(
                            "error writing to file {file_idx} (\"{:?}\")",
                            file_info.relative_filename
                        )
                    })?;
                debug_assert_eq!(written, to_write);
            }
            data.advance(to_write);
            if data.is_empty() {
                break;
            }

            offset_in_file = 0;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        path::Path,
        sync::{
            Mutex,
            atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        },
    };

    use bytes::Bytes;
    use clone_to_owned::CloneToOwned;
    use librqbit_core::constants::CHUNK_SIZE;
    use peer_binary_protocol::Piece;
    use sha1w::{ISha1, Sha1};

    use crate::{
        bitv::BitV,
        file_ops::FileOps,
        storage::TorrentStorage,
        torrent_state::{ManagedTorrentShared, TorrentMetadata},
    };

    struct CountingMemoryStorage {
        files: Mutex<HashMap<usize, Vec<u8>>>,
        read_calls: AtomicUsize,
        write_calls: AtomicUsize,
    }

    impl CountingMemoryStorage {
        fn new() -> Self {
            Self {
                files: Mutex::new(HashMap::new()),
                read_calls: AtomicUsize::new(0),
                write_calls: AtomicUsize::new(0),
            }
        }
    }

    impl TorrentStorage for CountingMemoryStorage {
        fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
            self.read_calls.fetch_add(1, Ordering::Relaxed);
            let g = self.files.lock().unwrap();
            let data = g
                .get(&file_id)
                .ok_or_else(|| anyhow::anyhow!("missing file {file_id}"))?;
            let start = usize::try_from(offset)?;
            let end = start + buf.len();
            if end > data.len() {
                anyhow::bail!("short read on file {file_id}");
            }
            buf.copy_from_slice(&data[start..end]);
            Ok(())
        }

        fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
            self.write_calls.fetch_add(1, Ordering::Relaxed);
            let mut g = self.files.lock().unwrap();
            let data = g.entry(file_id).or_default();
            let start = usize::try_from(offset)?;
            let end = start + buf.len();
            if data.len() < end {
                data.resize(end, 0);
            }
            data[start..end].copy_from_slice(buf);
            Ok(())
        }

        fn remove_file(&self, _file_id: usize, _filename: &Path) -> anyhow::Result<()> {
            Ok(())
        }

        fn ensure_file_length(&self, _file_id: usize, _length: u64) -> anyhow::Result<()> {
            Ok(())
        }

        fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
            Ok(Box::new(Self::new()))
        }

        fn remove_directory_if_empty(&self, _path: &Path) -> anyhow::Result<()> {
            Ok(())
        }

        fn init(
            &mut self,
            _shared: &ManagedTorrentShared,
            _metadata: &TorrentMetadata,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_file_ops_binary_search_and_short_circuit_initial_check() {
        let piece_len = CHUNK_SIZE as usize;
        // 20 files of 2048 bytes each = 40960 bytes (3 pieces of 16384, 16384, 8192)
        // Plus a 0-byte file in the middle to verify 0-byte files are skipped cleanly.
        let file_lengths: Vec<usize> = (0..21).map(|i| if i == 10 { 0 } else { 2048 }).collect();
        let total_len: usize = file_lengths.iter().sum();
        let payload: Vec<u8> = (0..total_len)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();

        let mut pieces = Vec::new();
        for chunk in payload.chunks(piece_len) {
            let mut h = Sha1::new();
            h.update(chunk);
            pieces.extend_from_slice(&h.finish());
        }

        let mut flist = vec![b'l'];
        for (i, &len) in file_lengths.iter().enumerate() {
            let fname = format!("f_{i:02}.bin");
            let entry = format!("d6:lengthi{len}e4:pathl{}:{fname}ee", fname.len());
            flist.extend_from_slice(entry.as_bytes());
        }
        flist.push(b'e');
        let mut torrent_bytes = Vec::new();
        torrent_bytes.extend_from_slice(b"d4:infod5:files");
        torrent_bytes.extend_from_slice(&flist);
        torrent_bytes.extend_from_slice(b"4:name4:test12:piece lengthi16384e6:pieces");
        torrent_bytes.extend_from_slice(format!("{}:", pieces.len()).as_bytes());
        torrent_bytes.extend_from_slice(&pieces);
        torrent_bytes.extend_from_slice(b"ee");

        let tb = Bytes::from(torrent_bytes);
        let parsed = librqbit_core::torrent_metainfo::torrent_from_bytes(&tb).unwrap();
        let owned = parsed.clone_to_owned(Some(&tb));
        let info = owned.info.data.validate().unwrap();
        let meta = TorrentMetadata::new(info, tb.clone(), owned.info.raw_bytes.0).unwrap();

        // 1. Fresh/empty storage: initial_check should short-circuit each piece after the first missing file
        let storage = CountingMemoryStorage::new();
        let fops = FileOps::new(&meta.info, &storage, &meta.file_infos);
        let progress = AtomicU64::new(0);
        let pause = AtomicBool::new(false);
        let have = fops.initial_check(&progress, &pause).unwrap();
        assert!(!have.as_slice()[0]);
        assert!(!have.as_slice()[1]);
        assert!(!have.as_slice()[2]);
        // Only 1 read call per piece (3 pieces total), not 20 read calls!
        assert_eq!(storage.read_calls.load(Ordering::Relaxed), 3);

        // 2. Write all pieces via write_chunk, verify 0-byte file (idx 10) is never written or read
        let peer = "127.0.0.1:1000".parse().unwrap();
        let lengths = *meta.lengths();
        for p in 0..lengths.total_pieces() {
            let p_idx = lengths.validate_piece_index(p).unwrap();
            let offset = p as usize * piece_len;
            let end = (offset + piece_len).min(payload.len());
            let slice = &payload[offset..end];
            let cinfo = lengths
                .chunk_info_from_received_data(p_idx, 0, u32::try_from(slice.len()).unwrap())
                .unwrap();
            let msg = Piece::from_data(p, 0, slice);
            fops.write_chunk(peer, &msg, &cinfo).unwrap();
            assert!(fops.check_piece(p_idx).unwrap());
            let mut rbuf = vec![0u8; slice.len()];
            fops.read_chunk(peer, &cinfo, &mut rbuf).unwrap();
            assert_eq!(&rbuf[..], slice);
        }
        assert!(!storage.files.lock().unwrap().contains_key(&10));

        // 3. Remove only file 0 (in piece 0); piece 0 should fail on first file, while pieces 1 and 2 still pass!
        storage.files.lock().unwrap().remove(&0);
        storage.read_calls.store(0, Ordering::Relaxed);
        let have2 = fops.initial_check(&progress, &pause).unwrap();
        assert!(!have2.as_slice()[0]);
        assert!(have2.as_slice()[1]);
        assert!(have2.as_slice()[2]);
    }
}
