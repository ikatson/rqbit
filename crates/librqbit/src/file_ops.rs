use std::{
    collections::BTreeMap,
    marker::PhantomData,
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::Context;
use buffers::{ByteBuf, ByteBufOwned};
use bytes::Bytes;
use librqbit_core::{
    Id32,
    lengths::{ChunkInfo, V2Lengths, ValidPieceIndex},
    torrent_metainfo::{V2FileInfo, ValidatedTorrentMetaV1Info, collect_v2_files},
};
use parking_lot::RwLock;
use peer_binary_protocol::{DoubleBufHelper, Piece};
use sha1w::{ISha1, Sha1};
use tracing::{debug, trace, warn};

use crate::{
    error::{Error as RqbitError, V2VerifyError},
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
    piece_layers: Arc<RwLock<Option<BTreeMap<Id32, Bytes>>>>,
    v2_files: Option<Vec<V2FileInfo<'a, ByteBufOwned>>>,
    v2_file_info_indices: Option<Vec<usize>>,
    phantom_data: PhantomData<Sha1>,
}

impl<'a> FileOps<'a> {
    pub fn new(
        torrent: &'a ValidatedTorrentMetaV1Info<ByteBufOwned>,
        files: &'a dyn TorrentStorage,
        file_infos: &'a FileInfos,
        piece_layers: Arc<RwLock<Option<BTreeMap<Id32, Bytes>>>>,
    ) -> Self {
        let v2_files = torrent.info().file_tree.as_ref().map(collect_v2_files);
        let v2_file_info_indices = v2_files.as_ref().map(|_| {
            file_infos
                .iter()
                .enumerate()
                .filter_map(|(idx, fi)| (!fi.attrs.padding).then_some(idx))
                .collect::<Vec<usize>>()
        });
        Self {
            torrent,
            files,
            file_infos,
            piece_layers,
            v2_files,
            v2_file_info_indices,
            phantom_data: PhantomData,
        }
    }

    /// Get the V2Lengths if this is a v2 torrent.
    fn v2_lengths(&self) -> Option<&V2Lengths> {
        self.torrent.v2_lengths()
    }

    fn is_hybrid(&self) -> bool {
        let info = self.torrent.info();
        let has_v1 = info.pieces.as_ref().is_some_and(|p| !p.as_ref().is_empty());
        let has_v2 = info.meta_version == Some(2) && info.file_tree.is_some();
        has_v1 && has_v2
    }

    /// Get the expected v2 piece hash for a given piece index.
    ///
    /// For single-piece files, the `pieces_root` IS the piece hash.
    /// For multi-piece files, looks up `piece_layers[pieces_root]` at the local piece offset.
    fn get_v2_piece_hash(
        &self,
        v2_lengths: &V2Lengths,
        piece_index: ValidPieceIndex,
    ) -> Result<Id32, V2VerifyError> {
        let v2_files = self.v2_files.as_ref().ok_or_else(|| {
            V2VerifyError::FileMappingMismatch("v2 torrent missing file_tree".into())
        })?;
        let (file_idx, _offset) = v2_lengths
            .file_for_piece(piece_index)
            .ok_or_else(|| V2VerifyError::PieceIndexOutOfRange(piece_index.get()))?;
        let local_piece = v2_lengths
            .local_piece_index(piece_index)
            .ok_or_else(|| V2VerifyError::PieceIndexOutOfRange(piece_index.get()))?;

        let file_info = v2_files.get(file_idx).ok_or_else(|| {
            V2VerifyError::FileMappingMismatch("file index out of range in v2 file tree".into())
        })?;

        let v2_file_pieces = v2_lengths.files();
        let num_pieces = v2_file_pieces
            .get(file_idx)
            .ok_or_else(|| V2VerifyError::FileMappingMismatch("file index out of range".into()))?
            .num_pieces;

        let pieces_root = file_info.entry.pieces_root.as_ref().ok_or_else(|| {
            V2VerifyError::FileMappingMismatch("v2 file missing pieces_root".into())
        })?;

        if num_pieces == 1 {
            // Single-piece file: pieces_root IS the piece hash.
            return Ok(*pieces_root);
        }

        // Multi-piece file: look up in piece_layers.
        let piece_layers = self.piece_layers.read();
        let layer_map = piece_layers
            .as_ref()
            .ok_or(V2VerifyError::PieceLayersMissing)?;
        let layer_data = layer_map.get(pieces_root).ok_or_else(|| {
            V2VerifyError::FileMappingMismatch("piece_layers missing entry for file".into())
        })?;

        let start = local_piece as usize * 32;
        let end = start + 32;
        if end > layer_data.len() {
            return Err(V2VerifyError::FileMappingMismatch(format!(
                "piece_layers entry too short for local piece index {local_piece}"
            )));
        }

        let mut piece_hash = [0u8; 32];
        piece_hash.copy_from_slice(&layer_data[start..end]);
        Ok(Id32::new(piece_hash))
    }

    fn v2_file_info_index(&self, v2_file_index: usize) -> Result<usize, V2VerifyError> {
        let v2_files = self.v2_files.as_ref().ok_or_else(|| {
            V2VerifyError::FileMappingMismatch("v2 torrent missing file_tree".into())
        })?;
        let indices = self.v2_file_info_indices.as_ref().ok_or_else(|| {
            V2VerifyError::FileMappingMismatch("v2 file mapping not available".into())
        })?;
        if indices.len() != v2_files.len() {
            return Err(V2VerifyError::FileMappingMismatch(format!(
                "v2/v1 file list mismatch: v2_files={} file_infos_non_padding={}",
                v2_files.len(),
                indices.len()
            )));
        }
        indices.get(v2_file_index).copied().ok_or_else(|| {
            V2VerifyError::FileMappingMismatch(
                "v2 file index out of range in file_infos mapping".into(),
            )
        })
    }

    /// Check a v2 piece using SHA-256 merkle verification.
    fn check_piece_v2(
        &self,
        v2_lengths: &V2Lengths,
        piece_index: ValidPieceIndex,
    ) -> Result<bool, V2VerifyError> {
        use librqbit_core::merkle;
        use sha1w::ISha256;

        let (v2_file_idx, offset_in_file) = v2_lengths
            .file_for_piece(piece_index)
            .ok_or_else(|| V2VerifyError::PieceIndexOutOfRange(piece_index.get()))?;
        let file_idx = self.v2_file_info_index(v2_file_idx)?;

        let piece_length = v2_lengths.piece_length(piece_index);
        let mut buf = vec![0u8; std::cmp::min(65536, piece_length as usize)];

        // Read piece data and hash each 16 KiB block with SHA-256.
        let blocks_per_piece = v2_lengths.piece_length_val() / merkle::MERKLE_BLOCK_SIZE;
        let actual_blocks = piece_length.div_ceil(merkle::MERKLE_BLOCK_SIZE) as usize;
        let mut block_hashes = Vec::with_capacity(actual_blocks);

        let fi = self
            .file_infos
            .get(file_idx)
            .ok_or_else(|| V2VerifyError::FileMappingMismatch("file index out of range".into()))?;

        let mut pos = offset_in_file;
        let mut remaining = piece_length as usize;

        while remaining > 0 {
            let block_size = std::cmp::min(remaining, merkle::MERKLE_BLOCK_SIZE as usize);
            buf.resize(block_size, 0);

            // Read the block.
            let to_read = block_size;
            let mut block_offset = 0;
            while block_offset < to_read {
                let chunk = std::cmp::min(buf.len(), to_read - block_offset);
                if fi.attrs.padding {
                    buf[block_offset..block_offset + chunk].fill(0);
                } else {
                    self.files
                        .pread_exact(file_idx, pos, &mut buf[block_offset..block_offset + chunk])
                        .map_err(|_| V2VerifyError::StorageReadFailure {
                            file_idx,
                            offset: pos,
                            len: chunk,
                        })?;
                }
                block_offset += chunk;
                pos += chunk as u64;
            }
            remaining -= block_size;

            let mut h = sha1w::Sha256::new();
            h.update(&buf[..block_size]);
            block_hashes.push(Id32::new(h.finish()));
        }

        let expected = self.get_v2_piece_hash(v2_lengths, piece_index)?;

        let result = merkle::verify_piece(&block_hashes, &expected, blocks_per_piece);

        if result {
            trace!("piece={} v2 hash matches", piece_index);
            Ok(true)
        } else {
            warn!("piece={} v2 hash does not match", piece_index);
            Err(V2VerifyError::MerkleMismatch(piece_index.get()))
        }
    }

    /// Check a v1 piece using SHA-1 flat-piece verification.
    fn check_piece_v1(&self, piece_index: ValidPieceIndex) -> anyhow::Result<bool> {
        let mut h = Sha1::new();
        let piece_length = self.torrent.lengths().piece_length(piece_index);
        let mut absolute_offset = self.torrent.lengths().piece_offset(piece_index);
        let mut buf = vec![0u8; std::cmp::min(65536, piece_length as usize)];

        let mut piece_remaining_bytes = piece_length as usize;

        for (file_idx, fi) in self.file_infos.iter().enumerate() {
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
            update_hash_from_file(
                file_idx,
                fi,
                absolute_offset,
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

    // Returns the bitvector with pieces we have.
    pub fn initial_check(&self, progress: &AtomicU64) -> anyhow::Result<BF> {
        // v2 fast path: iterate pieces, each maps to exactly one file.
        if let Some(v2_lengths) = self.v2_lengths() {
            if self.is_hybrid() {
                debug_assert!(
                    self.v2_lengths().is_some(),
                    "hybrid torrent expected to have v2_lengths"
                );
                let mut have_pieces = BF::from_boxed_slice(
                    vec![0u8; self.torrent.lengths().piece_bitfield_bytes()].into(),
                );

                for piece_info in self.torrent.lengths().iter_piece_infos() {
                    progress.fetch_add(piece_info.len as u64, Ordering::Relaxed);

                    match self.check_piece(piece_info.piece_index) {
                        Ok(true) => {
                            have_pieces.set(piece_info.piece_index.get() as usize, true);
                        }
                        Ok(false) => {}
                        Err(e) => {
                            debug!(
                                "error checking hybrid piece {}: {:#}",
                                piece_info.piece_index, e
                            );
                        }
                    }
                }

                return Ok(have_pieces);
            }

            let mut have_pieces = BF::from_boxed_slice(
                vec![0u8; self.torrent.lengths().piece_bitfield_bytes()].into(),
            );

            for file in v2_lengths.files() {
                for local_piece in 0..file.num_pieces {
                    let global_piece = file.first_piece_index + local_piece;
                    let Some(piece_index) =
                        self.torrent.lengths().validate_piece_index(global_piece)
                    else {
                        debug!(
                            "invalid v2 piece index {} during initial_check",
                            global_piece
                        );
                        continue;
                    };

                    let piece_len = v2_lengths.piece_length(piece_index);
                    progress.fetch_add(piece_len as u64, Ordering::Relaxed);

                    match self.check_piece_v2(v2_lengths, piece_index) {
                        Ok(true) => {
                            have_pieces.set(piece_index.get() as usize, true);
                        }
                        Ok(false) => {}
                        Err(V2VerifyError::MerkleMismatch(_)) => {}
                        Err(V2VerifyError::PieceLayersMissing) => {}
                        Err(e) => {
                            debug!(
                                "error checking v2 piece {}: {:#}",
                                piece_index,
                                anyhow::Error::new(RqbitError::from(e))
                            );
                        }
                    }
                }
            }

            return Ok(have_pieces);
        }

        // v1 path (existing logic).
        let mut have_pieces =
            BF::from_boxed_slice(vec![0u8; self.torrent.lengths().piece_bitfield_bytes()].into());
        let mut piece_files = Vec::<usize>::new();

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
            piece_files.clear();
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

                piece_files.push(current_file.index);

                let pos = current_file.processed_bytes;
                piece_remaining -= to_read_in_file;
                current_file.mark_processed_bytes(to_read_in_file as u64);

                if current_file.is_broken {
                    // no need to read.
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

        // Hybrid torrents must verify BOTH v1 (SHA-1) and v2 (SHA-256 merkle) hashes.
        // A piece that passes only one check is rejected.
        match (self.v2_lengths(), self.is_hybrid()) {
            (Some(v2_lengths), true) => {
                debug_assert!(
                    self.v2_lengths().is_some(),
                    "hybrid torrent expected to have v2_lengths"
                );
                let v2_ok = match self.check_piece_v2(v2_lengths, piece_index) {
                    Ok(v) => v,
                    Err(V2VerifyError::MerkleMismatch(_)) => false,
                    Err(V2VerifyError::PieceLayersMissing) => false,
                    Err(e) => return Err(anyhow::Error::new(RqbitError::from(e))),
                };
                let v1_ok = self.check_piece_v1(piece_index)?;
                Ok(v1_ok && v2_ok)
            }
            (Some(v2_lengths), false) => match self.check_piece_v2(v2_lengths, piece_index) {
                Ok(v) => Ok(v),
                Err(V2VerifyError::MerkleMismatch(_)) => Ok(false),
                Err(V2VerifyError::PieceLayersMissing) => Ok(false),
                Err(e) => Err(anyhow::Error::new(RqbitError::from(e))),
            },
            (None, _) => self.check_piece_v1(piece_index),
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

        // v2 fast path: piece belongs to exactly one file.
        if let Some(v2_lengths) = self.v2_lengths() {
            let (v2_file_idx, piece_offset_in_file) = v2_lengths
                .file_for_piece(chunk_info.piece_index)
                .context("v2 read_chunk: piece index out of range")?;
            let file_idx = self.v2_file_info_index(v2_file_idx)?;
            let offset = piece_offset_in_file + chunk_info.offset as u64;
            let fi = self
                .file_infos
                .get(file_idx)
                .context("v2 read_chunk: file index out of range")?;

            trace!(
                "v2 read_chunk: piece={}, handle={}, file_idx={}, offset={}",
                chunk_info.piece_index, who_sent, file_idx, offset
            );

            if fi.attrs.padding {
                result_buf[..chunk_info.size as usize].fill(0);
            } else {
                self.files
                    .pread_exact(
                        file_idx,
                        offset,
                        &mut result_buf[..chunk_info.size as usize],
                    )
                    .with_context(|| {
                        format!(
                            "v2 error reading {} bytes from file {file_idx} at offset {offset}",
                            chunk_info.size
                        )
                    })?;
            }
            return Ok(());
        }

        // v1 path.
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
        // v2 fast path: piece belongs to exactly one file.
        if let Some(v2_lengths) = self.v2_lengths() {
            let (v2_file_idx, piece_offset_in_file) = v2_lengths
                .file_for_piece(chunk_info.piece_index)
                .context("v2 write_chunk: piece index out of range")?;
            let file_idx = self.v2_file_info_index(v2_file_idx)?;
            let offset = piece_offset_in_file + chunk_info.offset as u64;
            let fi = self
                .file_infos
                .get(file_idx)
                .context("v2 write_chunk: file index out of range")?;

            trace!(
                "v2 write_chunk: piece={}, chunk={:?}, handle={}, file={}, offset={}",
                chunk_info.piece_index, chunk_info, who_sent, file_idx, offset
            );

            if !fi.attrs.padding {
                let data_helper = DoubleBufHelper::new(data.data().0, data.data().1);
                let to_write = chunk_info.size as usize;
                let slices = data_helper.as_ioslices(to_write);
                debug_assert_eq!(slices[0].len() + slices[1].len(), to_write);
                let written = self
                    .files
                    .pwrite_all_vectored(file_idx, offset, slices)
                    .with_context(|| {
                        format!(
                            "v2 error writing to file {file_idx} (\"{:?}\")",
                            fi.relative_filename
                        )
                    })?;
                debug_assert_eq!(written, to_write);
            }
            return Ok(());
        }

        // v1 path.
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
