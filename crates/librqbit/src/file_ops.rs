use std::{
    marker::PhantomData,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use buffers::{ByteBuf, ByteBufOwned};
use librqbit_core::{
    lengths::{ChunkInfo, Lengths, ValidPieceIndex},
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

const HASH_READ_BUF_LEN: usize = 65536;

#[allow(clippy::too_many_arguments)]
fn update_hash_from_file_abortable<Sha1: ISha1>(
    file_id: usize,
    file_info: &FileInfo,
    mut pos: u64,
    files: &dyn TorrentStorage,
    hash: &mut Sha1,
    buf: &mut [u8],
    mut bytes_to_read: usize,
    mut is_aborted: impl FnMut() -> bool,
    mut on_chunk: impl FnMut(usize),
) -> anyhow::Result<()> {
    let mut read = 0;
    while bytes_to_read > 0 {
        if is_aborted() {
            bail!("initial check paused");
        }
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
        on_chunk(chunk);
    }
    Ok(())
}

/// Resumable state for incremental initial checksum validation across bounded blocking slices.
/// Does not hold `Sha1` so it remains `Send + 'static` across all SHA1 backends.
pub(crate) struct InitialCheckState {
    have_pieces: BF,
    current_file_idx: usize,
    current_file_processed_bytes: u64,
    current_file_is_broken: bool,
    next_piece_idx: u32,
    total_pieces: u32,
    read_buffer: Vec<u8>,
}

impl InitialCheckState {
    pub fn new(lengths: &Lengths, file_infos: &FileInfos) -> anyhow::Result<Self> {
        if file_infos.is_empty() {
            bail!("empty input file list");
        }
        Ok(Self {
            have_pieces: BF::from_boxed_slice(vec![0u8; lengths.piece_bitfield_bytes()].into()),
            current_file_idx: 0,
            current_file_processed_bytes: 0,
            current_file_is_broken: false,
            next_piece_idx: 0,
            total_pieces: lengths.total_pieces(),
            read_buffer: vec![0u8; HASH_READ_BUF_LEN],
        })
    }

    pub fn into_have_pieces(self) -> BF {
        self.have_pieces
    }
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

    #[cfg(test)]
    pub fn initial_check(
        &self,
        progress: &AtomicU64,
        pause_requested: &std::sync::atomic::AtomicBool,
    ) -> anyhow::Result<BF> {
        let mut state = InitialCheckState::new(self.torrent.lengths(), self.file_infos)?;
        let done = self.initial_check_step(
            &mut state,
            progress,
            || pause_requested.load(Ordering::Relaxed),
            usize::MAX,
            Duration::MAX,
        )?;
        debug_assert!(done);
        Ok(state.into_have_pieces())
    }

    /// Processes pieces until all pieces are checked (`Ok(true)`) or either `max_bytes` of
    /// hashing or `max_duration` of wall time has elapsed (`Ok(false)`).
    pub fn initial_check_step(
        &self,
        state: &mut InitialCheckState,
        progress: &AtomicU64,
        mut is_aborted: impl FnMut() -> bool,
        max_bytes: usize,
        max_duration: Duration,
    ) -> anyhow::Result<bool> {
        let slice_start = Instant::now();
        let mut slice_bytes_hashed: usize = 0;

        while state.next_piece_idx < state.total_pieces {
            if is_aborted() {
                bail!("initial check paused");
            }

            let piece_index = self
                .torrent
                .lengths()
                .validate_piece_index(state.next_piece_idx)
                .context("bug: invalid piece index")?;
            let piece_len = self.torrent.lengths().piece_length(piece_index);
            state.next_piece_idx += 1;

            let mut computed_hash = Sha1::new();
            let mut piece_remaining = piece_len as usize;
            let mut some_files_broken = false;

            while piece_remaining > 0 {
                let current_fi = self
                    .file_infos
                    .get(state.current_file_idx)
                    .context("broken torrent metadata")?;
                let current_remaining = current_fi.len - state.current_file_processed_bytes;
                let mut to_read_in_file: usize =
                    std::cmp::min(current_remaining, piece_remaining as u64).try_into()?;

                // Keep changing the current file to next until we find a file that has greater than 0 length.
                while to_read_in_file == 0 {
                    state.current_file_idx += 1;
                    state.current_file_processed_bytes = 0;
                    state.current_file_is_broken = false;

                    let next_fi = self
                        .file_infos
                        .get(state.current_file_idx)
                        .context("broken torrent metadata")?;
                    to_read_in_file =
                        std::cmp::min(next_fi.len, piece_remaining as u64).try_into()?;
                }

                let current_fi = &self.file_infos[state.current_file_idx];
                let pos = state.current_file_processed_bytes;
                piece_remaining -= to_read_in_file;
                state.current_file_processed_bytes += to_read_in_file as u64;

                if state.current_file_is_broken {
                    // No need to read from an already-broken file.
                    some_files_broken = true;
                    progress.fetch_add(to_read_in_file as u64, Ordering::Relaxed);
                    continue;
                }

                let mut accounted = 0usize;
                if let Err(err) = update_hash_from_file_abortable(
                    state.current_file_idx,
                    current_fi,
                    pos,
                    self.files,
                    &mut computed_hash,
                    &mut state.read_buffer,
                    to_read_in_file,
                    &mut is_aborted,
                    |chunk| {
                        accounted += chunk;
                        slice_bytes_hashed = slice_bytes_hashed.saturating_add(chunk);
                        progress.fetch_add(chunk as u64, Ordering::Relaxed);
                    },
                ) {
                    if is_aborted() {
                        return Err(err);
                    }
                    if accounted < to_read_in_file {
                        progress.fetch_add((to_read_in_file - accounted) as u64, Ordering::Relaxed);
                    }
                    debug!(
                        "error reading from file {} ({:?}) at {}: {:#}",
                        state.current_file_idx, current_fi.relative_filename, pos, &err
                    );
                    state.current_file_is_broken = true;
                    some_files_broken = true;
                }
            }

            if !some_files_broken {
                if self
                    .torrent
                    .info()
                    .compare_hash(piece_index.get(), computed_hash.finish())
                    .context(
                        "bug: either torrent info broken or we have a bug - piece index invalid",
                    )?
                {
                    state.have_pieces.set(piece_index.get() as usize, true);
                }
            } else {
                trace!("piece {} had errors, marking as needed", piece_index);
            }

            if state.next_piece_idx < state.total_pieces
                && (slice_bytes_hashed >= max_bytes || slice_start.elapsed() >= max_duration)
            {
                return Ok(false);
            }
        }

        Ok(true)
    }

    pub fn check_piece(&self, piece_index: ValidPieceIndex) -> anyhow::Result<bool> {
        let piece_length = self.torrent.lengths().piece_length(piece_index);
        let mut buf = vec![0u8; std::cmp::min(HASH_READ_BUF_LEN, piece_length as usize)];
        self.check_piece_with_buf_abortable(piece_index, &mut buf, || false)
    }

    pub fn check_piece_with_buf_abortable(
        &self,
        piece_index: ValidPieceIndex,
        buf: &mut [u8],
        mut is_aborted: impl FnMut() -> bool,
    ) -> anyhow::Result<bool> {
        if cfg!(feature = "_disable_disk_write_net_benchmark") {
            return Ok(true);
        }

        let mut h = Sha1::new();
        let piece_length = self.torrent.lengths().piece_length(piece_index);
        let mut absolute_offset = self.torrent.lengths().piece_offset(piece_index);

        let mut piece_remaining_bytes = piece_length as usize;

        for (file_idx, fi) in self.file_infos.iter().enumerate() {
            if is_aborted() {
                bail!("initial check paused");
            }
            let file_len = fi.len;
            if absolute_offset > file_len {
                absolute_offset -= file_len;
                continue;
            }
            let file_remaining_len = file_len - absolute_offset;

            let to_read_in_file: usize =
                std::cmp::min(file_remaining_len, piece_remaining_bytes as u64).try_into()?;
            trace!(
                "piece={}, file_idx={}, seeking to {}",
                piece_index, file_idx, absolute_offset,
            );
            update_hash_from_file_abortable(
                file_idx,
                fi,
                absolute_offset,
                self.files,
                &mut h,
                buf,
                to_read_in_file,
                &mut is_aborted,
                |_| {},
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

            absolute_offset = 0;
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
        let mut absolute_offset = self.torrent.lengths().chunk_absolute_offset(chunk_info);
        let mut buf = result_buf;

        for (file_idx, file_info) in self.file_infos.iter().enumerate() {
            let file_len = file_info.len;
            if absolute_offset > file_len {
                absolute_offset -= file_len;
                continue;
            }
            let file_remaining_len = file_len - absolute_offset;
            let to_read_in_file = std::cmp::min(file_remaining_len, buf.len() as u64).try_into()?;

            trace!(
                "piece={}, handle={}, file_idx={}, seeking to {}. To read chunk: {:?}",
                chunk_info.piece_index, who_sent, file_idx, absolute_offset, &chunk_info
            );
            if file_info.attrs.padding {
                buf[..to_read_in_file].fill(0);
            } else {
                self.files
                    .pread_exact(file_idx, absolute_offset, &mut buf[..to_read_in_file])
                    .with_context(|| {
                        format!("error reading {file_idx} bytes, file_id: {to_read_in_file}")
                    })?;
            }

            buf = &mut buf[to_read_in_file..];

            if buf.is_empty() {
                break;
            }

            absolute_offset = 0;
        }

        Ok(())
    }

    pub fn write_chunk(
        &self,
        who_sent: PeerHandle,
        data: &Piece<ByteBuf<'a>>,
        chunk_info: &ChunkInfo,
    ) -> anyhow::Result<()> {
        let mut absolute_offset = self.torrent.lengths().chunk_absolute_offset(chunk_info);
        let mut data = DoubleBufHelper::new(data.data().0, data.data().1);

        for (file_idx, file_info) in self.file_infos.iter().enumerate() {
            let file_len = file_info.len;
            if absolute_offset > file_len {
                absolute_offset -= file_len;
                continue;
            }

            let remaining_len = file_len - absolute_offset;
            let to_write = std::cmp::min(data.len() as u64, remaining_len).try_into()?;

            trace!(
                "piece={}, chunk={:?}, handle={}, begin={}, file={}, writing {} bytes at {}",
                chunk_info.piece_index,
                chunk_info,
                who_sent,
                chunk_info.offset,
                file_idx,
                to_write,
                absolute_offset
            );
            let slices = data.as_ioslices(to_write);
            debug_assert_eq!(slices[0].len() + slices[1].len(), to_write);
            if !file_info.attrs.padding {
                let written = self
                    .files
                    .pwrite_all_vectored(file_idx, absolute_offset, slices)
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

            absolute_offset = 0;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Seek, SeekFrom, Write},
        sync::atomic::{AtomicBool, AtomicU64, Ordering},
        time::Duration,
    };

    use buffers::ByteBufOwned;
    use bytes::Bytes;
    use librqbit_core::{
        Id20,
        torrent_metainfo::{TorrentMetaV1File, TorrentMetaV1Info},
    };
    use sha1w::{ISha1, Sha1};
    use tempfile::TempDir;

    use crate::{
        file_ops::{FileOps, InitialCheckState},
        spawn_utils::BlockingSpawner,
        storage::{StorageFactory, StorageFactoryExt, filesystem::FilesystemStorageFactory},
        torrent_state::{ManagedTorrentOptions, ManagedTorrentShared, TorrentMetadata},
    };

    #[tokio::test]
    async fn test_initial_check_step_matches_full_check_across_broken_and_empty_files() {
        let dir = TempDir::new().unwrap();
        let piece_len = 16384u32;

        // Deterministic file list with 0-byte files at start, middle, and end, plus
        // cross-piece intact, truncated, and corrupted files.
        let spec: &[(&str, Vec<u8>)] = &[
            ("0_empty_start.bin", vec![]),
            ("1_intact_a.bin", vec![0x11u8; 25_000]),
            ("2_truncated.bin", vec![0x22u8; 40_000]),
            ("3_empty_mid.bin", vec![]),
            ("4_corrupted.bin", vec![0x33u8; 35_000]),
            ("5_intact_b.bin", vec![0x44u8; 50_000]),
            ("6_empty_end.bin", vec![]),
        ];

        let mut all_bytes = Vec::new();
        let mut files = Vec::new();
        for (name, data) in spec {
            std::fs::write(dir.path().join(name), data).unwrap();
            all_bytes.extend_from_slice(data);
            files.push(TorrentMetaV1File {
                length: data.len() as u64,
                path: vec![ByteBufOwned::from(name.as_bytes())],
                attr: None,
                sha1: None,
                symlink_path: None,
            });
        }

        let mut piece_hashes = Vec::new();
        for chunk in all_bytes.chunks(piece_len as usize) {
            let mut h = Sha1::new();
            h.update(chunk);
            piece_hashes.extend_from_slice(&h.finish());
        }

        let info = TorrentMetaV1Info {
            name: Some(ByteBufOwned::from(&b"multi"[..])),
            pieces: piece_hashes.into(),
            piece_length: piece_len,
            length: None,
            md5sum: None,
            files: Some(files),
            attr: None,
            sha1: None,
            symlink_path: None,
            private: false,
        };
        let mut info_raw = Vec::new();
        bencode::bencode_serialize_to_writer(&info, &mut info_raw).unwrap();
        let info_hash = Id20::new({
            let mut h = Sha1::new();
            h.update(&info_raw);
            h.finish()
        });
        let info_bytes = Bytes::from(info_raw);
        let metadata =
            TorrentMetadata::new(info.validate().unwrap(), Bytes::new(), info_bytes).unwrap();

        // Truncate file 2 (leaving its first piece intact) and corrupt one byte inside file 4.
        std::fs::write(dir.path().join("2_truncated.bin"), vec![0x22u8; 20_000]).unwrap();
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(dir.path().join("4_corrupted.bin"))
                .unwrap();
            f.seek(SeekFrom::Start(20_000)).unwrap();
            f.write_all(&[0xFF]).unwrap();
        }

        let shared = ManagedTorrentShared {
            id: 0,
            span: tracing::Span::none(),
            info_hash,
            trackers: Default::default(),
            spawner: BlockingSpawner::new(1),
            peer_id: Default::default(),
            storage_factory: FilesystemStorageFactory::default().boxed(),
            options: ManagedTorrentOptions {
                force_tracker_interval: None,
                peer_connect_timeout: None,
                peer_read_write_timeout: None,
                peer_max_request_window: None,
                allow_overwrite: true,
                output_folder: dir.path().to_owned(),
                ratelimits: Default::default(),
                initial_peers: Vec::new(),
                peer_limit: None,
                #[cfg(feature = "disable-upload")]
                _disable_upload: false,
            },
            connector: crate::stream_connect::StreamConnector::new(
                crate::stream_connect::StreamConnectorArgs {
                    enable_tcp: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .into(),
            session: std::sync::Weak::new(),
            magnet_name: None,
            client_name_and_version: String::new(),
        };

        let storage = FilesystemStorageFactory::default()
            .create_and_init(&shared, &metadata)
            .unwrap();
        let fo = FileOps::new(&metadata.info, &storage, &metadata.file_infos);

        let full_progress = AtomicU64::new(0);
        let pause = AtomicBool::new(false);
        let full_bf = fo.initial_check(&full_progress, &pause).unwrap();

        // Step 1 piece per slice (`max_bytes = 1`) and verify exact equivalence against
        // both `initial_check` and independent stateless `check_piece` calls.
        let stepped_progress = AtomicU64::new(0);
        let mut state = InitialCheckState::new(metadata.lengths(), &metadata.file_infos).unwrap();
        let mut steps = 0usize;
        loop {
            steps += 1;
            let done = fo
                .initial_check_step(&mut state, &stepped_progress, || false, 1, Duration::MAX)
                .unwrap();
            if done {
                break;
            }
        }
        let stepped_bf = state.into_have_pieces();

        // 9 of 10 pieces perform disk reads (`max_bytes = 1`); the 1 piece entirely inside
        // the already-marked-broken tail of `2_truncated.bin` is skipped without I/O.
        assert_eq!(steps, 9);
        assert_eq!(stepped_bf.as_raw_slice(), full_bf.as_raw_slice());
        for p in 0..metadata.lengths().total_pieces() {
            let valid = metadata.lengths().validate_piece_index(p).unwrap();
            assert_eq!(
                stepped_bf[p as usize],
                matches!(fo.check_piece(valid), Ok(true)),
                "mismatch at piece {p}"
            );
        }
        assert_eq!(
            stepped_progress.load(Ordering::Relaxed),
            metadata.lengths().total_length()
        );
        assert_eq!(
            full_progress.load(Ordering::Relaxed),
            metadata.lengths().total_length()
        );
        // Intact head and intact tail pieces must be marked present while broken middle pieces are absent.
        assert!(stepped_bf[0]);
        assert!(stepped_bf[(metadata.lengths().total_pieces() - 1) as usize]);
        assert!(stepped_bf.count_ones() < metadata.lengths().total_pieces() as usize);
    }
}
