use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::Context;
use buffers::{ByteBuf, ByteBufOwned};
use librqbit_core::{
    lengths::{ChunkInfo, ValidPieceIndex},
    metainfo_v2::{PieceVerdict, PieceVerifier},
    torrent_metainfo::ValidatedTorrentMetaV1Info,
};
use peer_binary_protocol::{DoubleBufHelper, Piece};
use tracing::{debug, trace, warn};

use crate::{
    file_info::FileInfo,
    storage::TorrentStorage,
    type_aliases::{BF, FileInfos, PeerHandle},
};

/// Returned by [`FileOps::initial_check`] when it stopped because a pause was requested.
#[derive(Debug)]
pub(crate) struct InitialCheckPaused;

impl std::fmt::Display for InitialCheckPaused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("initial check paused")
    }
}

impl std::error::Error for InitialCheckPaused {}

/// The parts of files that hold the `len` bytes at torrent offset `start`, as
/// (file index, file, offset within the file, length).
///
/// Files are in torrent order with increasing offsets. In v1 and hybrid
/// torrents they follow each other without gaps; in v2-only torrents each one
/// starts on a piece boundary, and no piece or chunk covers the gap after a
/// file, because the file's last piece is short.
fn file_slices(
    file_infos: &[FileInfo],
    start: u64,
    len: u64,
) -> impl Iterator<Item = (usize, &FileInfo, u64, u64)> {
    let end = start.saturating_add(len);
    let first = file_infos.partition_point(|fi| fi.offset_in_torrent + fi.len <= start);
    file_infos
        .iter()
        .enumerate()
        .skip(first)
        .take_while(move |(_, fi)| fi.offset_in_torrent < end)
        .filter_map(move |(idx, fi)| {
            let from = start.max(fi.offset_in_torrent);
            let to = end.min(fi.offset_in_torrent + fi.len);
            (to > from).then(|| (idx, fi, from - fi.offset_in_torrent, to - from))
        })
}

/// Feed `len` bytes of a file, starting at `pos`, to `verifier`, reading
/// through `buf`. Padding files read as zeros.
fn feed_from_file(
    file_id: usize,
    file_info: &FileInfo,
    mut pos: u64,
    mut len: u64,
    files: &dyn TorrentStorage,
    verifier: &mut PieceVerifier,
    buf: &mut [u8],
) -> anyhow::Result<()> {
    let mut read = 0u64;
    while len > 0 {
        let chunk = usize::try_from(len).unwrap_or(usize::MAX).min(buf.len());
        let Some(buf) = buf.get_mut(..chunk) else {
            anyhow::bail!("bug: read buffer is too small");
        };
        if file_info.attrs.padding {
            buf.fill(0);
        } else {
            files.pread_exact(file_id, pos, buf).with_context(|| {
                format!("failed reading chunk of size {chunk}, read so far {read}")
            })?;
        }
        verifier.update(buf);
        len -= chunk as u64;
        read += chunk as u64;
        pos += chunk as u64;
    }
    Ok(())
}

pub(crate) struct FileOps<'a> {
    torrent: &'a ValidatedTorrentMetaV1Info<ByteBufOwned>,
    files: &'a dyn TorrentStorage,
    file_infos: &'a FileInfos,
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
        }
    }

    // Returns the bitvector with pieces we have.
    pub fn initial_check(
        &self,
        progress: &AtomicU64,
        pause_requested: &AtomicBool,
    ) -> anyhow::Result<BF> {
        let lengths = self.torrent.lengths();
        let mut have_pieces =
            BF::from_boxed_slice(vec![0u8; lengths.piece_bitfield_bytes()].into());
        // A file that failed to read once (usually: it does not exist yet) is
        // not read again; the pieces it is part of are needed.
        let mut broken_files = vec![false; self.file_infos.len()];
        let mut read_buffer = vec![0u8; 65536];

        for piece_info in lengths.iter_piece_infos() {
            if pause_requested.load(Ordering::Relaxed) {
                return Err(InitialCheckPaused.into());
            }
            progress.fetch_add(piece_info.len as u64, Ordering::Relaxed);

            // None: a v2 piece whose piece layer is not known yet. It cannot be
            // trusted, so it is needed.
            let Some(mut verifier) = self.torrent.piece_verifier(piece_info.piece_index) else {
                continue;
            };
            let mut readable = true;
            for (idx, fi, pos, len) in file_slices(
                self.file_infos,
                lengths.piece_offset(piece_info.piece_index),
                piece_info.len.into(),
            ) {
                let Some(broken) = broken_files.get_mut(idx) else {
                    anyhow::bail!("bug: file index {idx} out of range");
                };
                if *broken {
                    readable = false;
                    break;
                }
                if let Err(err) = feed_from_file(
                    idx,
                    fi,
                    pos,
                    len,
                    self.files,
                    &mut verifier,
                    &mut read_buffer,
                ) {
                    debug!(
                        "error reading from file {} ({:?}) at {}: {:#}",
                        idx, fi.relative_filename, pos, &err
                    );
                    *broken = true;
                    readable = false;
                    break;
                }
            }

            if !readable {
                trace!(
                    "piece {} had errors, marking as needed",
                    piece_info.piece_index
                );
                continue;
            }

            match verifier.verdict() {
                PieceVerdict::Good => {
                    have_pieces.set(piece_info.piece_index.get() as usize, true);
                }
                PieceVerdict::Bad => {}
                PieceVerdict::Inconsistent => anyhow::bail!(
                    "piece {}: the v1 and v2 hashes of this hybrid torrent disagree",
                    piece_info.piece_index
                ),
            }
        }

        Ok(have_pieces)
    }

    /// Read a piece back from storage and check it against the torrent's
    /// hashes. Errors if the piece cannot be read or verified, or if the v1
    /// and v2 hashes of a hybrid torrent disagree about it.
    pub fn check_piece(&self, piece_index: ValidPieceIndex) -> anyhow::Result<bool> {
        if cfg!(feature = "_disable_disk_write_net_benchmark") {
            return Ok(true);
        }

        let lengths = self.torrent.lengths();
        let mut verifier = self.torrent.piece_verifier(piece_index).with_context(|| {
            format!("piece {piece_index} cannot be verified: its piece layer is not known")
        })?;
        let piece_length = lengths.piece_length(piece_index);
        let piece_offset = lengths.piece_offset(piece_index);
        let mut buf = vec![0u8; std::cmp::min(65536, piece_length as usize)];

        for (file_idx, fi, pos, len) in
            file_slices(self.file_infos, piece_offset, piece_length.into())
        {
            trace!(
                "piece={}, file_idx={}, seeking to {}",
                piece_index, file_idx, pos,
            );
            feed_from_file(file_idx, fi, pos, len, self.files, &mut verifier, &mut buf)
                .with_context(|| {
                    format!(
                        "error reading {len} bytes, file_id: {file_idx} (\"{:?}\")",
                        fi.relative_filename
                    )
                })?;
        }

        match verifier.verdict() {
            PieceVerdict::Good => {
                trace!("piece={} hash matches", piece_index);
                Ok(true)
            }
            PieceVerdict::Bad => {
                warn!(
                    piece_length,
                    piece_offset, "the piece={} hash does not match", piece_index
                );
                Ok(false)
            }
            PieceVerdict::Inconsistent => anyhow::bail!(
                "piece {piece_index}: the v1 and v2 hashes of this hybrid torrent disagree"
            ),
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
        let mut buf = result_buf;

        for (file_idx, file_info, pos, len) in
            file_slices(self.file_infos, absolute_offset, chunk_info.size.into())
        {
            trace!(
                "piece={}, handle={}, file_idx={}, seeking to {}. To read chunk: {:?}",
                chunk_info.piece_index, who_sent, file_idx, pos, &chunk_info
            );
            let to_read_in_file: usize = len.try_into()?;
            let Some((dst, rest)) = buf.split_at_mut_checked(to_read_in_file) else {
                anyhow::bail!("bug: chunk spans more bytes than its size");
            };
            if file_info.attrs.padding {
                dst.fill(0);
            } else {
                self.files
                    .pread_exact(file_idx, pos, dst)
                    .with_context(|| {
                        format!("error reading {to_read_in_file} bytes, file_id: {file_idx}")
                    })?;
            }
            buf = rest;
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

        for (file_idx, file_info, pos, len) in
            file_slices(self.file_infos, absolute_offset, data.len() as u64)
        {
            let to_write: usize = len.try_into()?;
            trace!(
                "piece={}, chunk={:?}, handle={}, begin={}, file={}, writing {} bytes at {}",
                chunk_info.piece_index,
                chunk_info,
                who_sent,
                chunk_info.offset,
                file_idx,
                to_write,
                pos
            );
            let slices = data.as_ioslices(to_write);
            debug_assert_eq!(slices[0].len() + slices[1].len(), to_write);
            if !file_info.attrs.padding {
                let written = self
                    .files
                    .pwrite_all_vectored(file_idx, pos, slices)
                    .with_context(|| {
                        format!(
                            "error writing to file {file_idx} (\"{:?}\")",
                            file_info.relative_filename
                        )
                    })?;
                debug_assert_eq!(written, to_write);
            }
            data.advance(to_write);
        }

        Ok(())
    }
}
