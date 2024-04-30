use std::{
    marker::PhantomData,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::Context;
use buffers::ByteBufOwned;
use librqbit_core::{
    lengths::{ChunkInfo, Lengths, ValidPieceIndex},
    torrent_metainfo::TorrentMetaV1Info,
};
use peer_binary_protocol::Piece;
use sha1w::{ISha1, Sha1};
use tracing::{debug, trace, warn};

use crate::{
    file_info::FileInfo,
    storage::TorrentStorage,
    type_aliases::{FileInfos, PeerHandle, BF},
};

pub(crate) struct InitialCheckResults {
    // A piece as flags based on these dimensions:
    // - if the asked for it or not (only_files)
    // - if we have it downloaded and verified
    // - if we need to queue it for downloading
    //   this one depends if we queued it already or not.

    // The pieces we have downloaded.
    pub have_pieces: BF,
    // The pieces that the user selected to download.
    pub selected_pieces: BF,

    // How many bytes we have. This can be MORE than "total_selected_bytes",
    // if we downloaded some pieces, and later the "only_files" was changed.
    pub have_bytes: u64,
    // How many bytes we need to download.
    pub needed_bytes: u64,

    // How many bytes are in selected pieces.
    // If all selected, this must be equal to total torrent length.
    pub selected_bytes: u64,
}

pub fn update_hash_from_file<Sha1: ISha1>(
    file_id: usize,
    mut pos: u64,
    files: &dyn TorrentStorage,
    hash: &mut Sha1,
    buf: &mut [u8],
    mut bytes_to_read: usize,
) -> anyhow::Result<()> {
    let mut read = 0;
    while bytes_to_read > 0 {
        let chunk = std::cmp::min(buf.len(), bytes_to_read);
        files
            .pread_exact(file_id, pos, &mut buf[..chunk])
            .with_context(|| format!("failed reading chunk of size {chunk}, read so far {read}"))?;
        bytes_to_read -= chunk;
        read += chunk;
        pos += chunk as u64;
        hash.update(&buf[..chunk]);
    }
    Ok(())
}

pub(crate) struct FileOps<'a> {
    torrent: &'a TorrentMetaV1Info<ByteBufOwned>,
    files: &'a dyn TorrentStorage,
    file_infos: &'a FileInfos,
    lengths: &'a Lengths,
    phantom_data: PhantomData<Sha1>,
}

impl<'a> FileOps<'a> {
    pub fn new(
        torrent: &'a TorrentMetaV1Info<ByteBufOwned>,
        files: &'a dyn TorrentStorage,
        file_infos: &'a FileInfos,
        lengths: &'a Lengths,
    ) -> Self {
        Self {
            torrent,
            files,
            file_infos,
            lengths,
            phantom_data: PhantomData,
        }
    }

    pub fn initial_check(
        &self,
        only_files: Option<&[usize]>,
        progress: &AtomicU64,
    ) -> anyhow::Result<InitialCheckResults> {
        let mut needed_pieces =
            BF::from_boxed_slice(vec![0u8; self.lengths.piece_bitfield_bytes()].into());
        let mut have_pieces = needed_pieces.clone();
        let mut selected_pieces = needed_pieces.clone();

        let mut have_bytes = 0u64;
        let mut needed_bytes = 0u64;
        let mut total_selected_bytes = 0u64;
        let mut piece_files = Vec::<usize>::new();

        #[derive(Debug)]
        struct CurrentFile<'a> {
            index: usize,
            fi: &'a FileInfo,
            full_file_required: bool,
            processed_bytes: u64,
            is_broken: bool,
        }
        impl<'a> CurrentFile<'a> {
            fn remaining(&self) -> u64 {
                self.fi.len - self.processed_bytes
            }
            fn mark_processed_bytes(&mut self, bytes: u64) {
                self.processed_bytes += bytes
            }
        }
        let mut file_iterator = self.file_infos.iter().enumerate().map(|(idx, fi)| {
            let full_file_required = if let Some(only_files) = only_files {
                only_files.contains(&idx)
            } else {
                true
            };
            CurrentFile {
                index: idx,
                fi,
                full_file_required,
                processed_bytes: 0,
                is_broken: false,
            }
        });

        let mut current_file = file_iterator
            .next()
            .ok_or_else(|| anyhow::anyhow!("empty input file list"))?;

        let mut read_buffer = vec![0u8; 65536];

        for piece_info in self.lengths.iter_piece_infos() {
            piece_files.clear();
            let mut computed_hash = Sha1::new();
            let mut piece_remaining = piece_info.len as usize;
            let mut some_files_broken = false;
            let mut piece_selected = current_file.full_file_required;
            progress.fetch_add(piece_info.len as u64, Ordering::Relaxed);

            while piece_remaining > 0 {
                let mut to_read_in_file: usize =
                    std::cmp::min(current_file.remaining(), piece_remaining as u64).try_into()?;

                // Keep changing the current file to next until we find a file that has greater than 0 length.
                while to_read_in_file == 0 {
                    current_file = file_iterator
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("broken torrent metadata"))?;

                    piece_selected |= current_file.full_file_required;

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

            if piece_selected {
                total_selected_bytes += piece_info.len as u64;
                selected_pieces.set(piece_info.piece_index.get() as usize, true);
            }

            if piece_selected && some_files_broken {
                trace!(
                    "piece {} had errors, marking as needed",
                    piece_info.piece_index
                );

                needed_bytes += piece_info.len as u64;
                continue;
            }

            if self
                .torrent
                .compare_hash(piece_info.piece_index.get(), computed_hash.finish())
                .context("bug: either torrent info broken or we have a bug - piece index invalid")?
            {
                trace!(
                    "piece {} is fine, not marking as needed",
                    piece_info.piece_index
                );
                have_bytes += piece_info.len as u64;
                have_pieces.set(piece_info.piece_index.get() as usize, true);
            } else if piece_selected {
                trace!(
                    "piece {} hash does not match, marking as needed",
                    piece_info.piece_index
                );
                needed_bytes += piece_info.len as u64;
                needed_pieces.set(piece_info.piece_index.get() as usize, true);
            } else {
                trace!(
                "piece {} hash does not match, but it is not required by any of the requested files, ignoring",
                piece_info.piece_index
            );
            }
        }

        Ok(InitialCheckResults {
            have_pieces,
            selected_pieces,
            have_bytes,
            needed_bytes,
            selected_bytes: total_selected_bytes,
        })
    }

    pub fn check_piece(
        &self,
        who_sent: PeerHandle,
        piece_index: ValidPieceIndex,
        last_received_chunk: &ChunkInfo,
    ) -> anyhow::Result<bool> {
        let mut h = Sha1::new();
        let piece_length = self.lengths.piece_length(piece_index);
        let mut absolute_offset = self.lengths.piece_offset(piece_index);
        let mut buf = vec![0u8; std::cmp::min(65536, piece_length as usize)];

        let mut piece_remaining_bytes = piece_length as usize;

        for (file_idx, (name, file_len)) in self.torrent.iter_filenames_and_lengths()?.enumerate() {
            if absolute_offset > file_len {
                absolute_offset -= file_len;
                continue;
            }
            let file_remaining_len = file_len - absolute_offset;

            let to_read_in_file: usize =
                std::cmp::min(file_remaining_len, piece_remaining_bytes as u64).try_into()?;
            trace!(
                "piece={}, handle={}, file_idx={}, seeking to {}. Last received chunk: {:?}",
                piece_index,
                who_sent,
                file_idx,
                absolute_offset,
                &last_received_chunk
            );
            update_hash_from_file(
                file_idx,
                absolute_offset,
                self.files,
                &mut h,
                &mut buf,
                to_read_in_file,
            )
            .with_context(|| {
                format!("error reading {to_read_in_file} bytes, file_id: {file_idx} (\"{name:?}\")")
            })?;

            piece_remaining_bytes -= to_read_in_file;

            if piece_remaining_bytes == 0 {
                break;
            }

            absolute_offset = 0;
        }

        match self.torrent.compare_hash(piece_index.get(), h.finish()) {
            Some(true) => {
                trace!("piece={} hash matches", piece_index);
                Ok(true)
            }
            Some(false) => {
                warn!("the piece={} hash does not match", piece_index);
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
        let mut absolute_offset = self.lengths.chunk_absolute_offset(chunk_info);
        let mut buf = result_buf;

        for (file_idx, file_len) in self.torrent.iter_file_lengths()?.enumerate() {
            if absolute_offset > file_len {
                absolute_offset -= file_len;
                continue;
            }
            let file_remaining_len = file_len - absolute_offset;
            let to_read_in_file = std::cmp::min(file_remaining_len, buf.len() as u64).try_into()?;

            trace!(
                "piece={}, handle={}, file_idx={}, seeking to {}. To read chunk: {:?}",
                chunk_info.piece_index,
                who_sent,
                file_idx,
                absolute_offset,
                &chunk_info
            );
            self.files
                .pread_exact(file_idx, absolute_offset, &mut buf[..to_read_in_file])
                .with_context(|| {
                    format!("error reading {file_idx} bytes, file_id: {to_read_in_file}")
                })?;

            buf = &mut buf[to_read_in_file..];

            if buf.is_empty() {
                break;
            }

            absolute_offset = 0;
        }

        Ok(())
    }

    pub fn write_chunk<ByteBuf>(
        &self,
        who_sent: PeerHandle,
        data: &Piece<ByteBuf>,
        chunk_info: &ChunkInfo,
    ) -> anyhow::Result<()>
    where
        ByteBuf: AsRef<[u8]>,
    {
        let mut buf = data.block.as_ref();
        let mut absolute_offset = self.lengths.chunk_absolute_offset(chunk_info);

        for (file_idx, (name, file_len)) in self.torrent.iter_filenames_and_lengths()?.enumerate() {
            if absolute_offset > file_len {
                absolute_offset -= file_len;
                continue;
            }

            let remaining_len = file_len - absolute_offset;
            let to_write = std::cmp::min(buf.len() as u64, remaining_len).try_into()?;

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
            self.files
                .pwrite_all(file_idx, absolute_offset, &buf[..to_write])
                .with_context(|| format!("error writing to file {file_idx} (\"{name:?}\")"))?;
            buf = &buf[to_write..];
            if buf.is_empty() {
                break;
            }

            absolute_offset = 0;
        }

        Ok(())
    }
}
