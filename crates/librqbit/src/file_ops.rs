use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    marker::PhantomData,
    sync::Arc,
};

use anyhow::Context;
use buffers::ByteString;
use librqbit_core::{
    lengths::{ChunkInfo, Lengths, ValidPieceIndex},
    torrent_metainfo::{FileIteratorName, TorrentMetaV1Info},
};
use log::{debug, trace, warn};
use parking_lot::Mutex;
use peer_binary_protocol::Piece;
use sha1w::ISha1;

use crate::type_aliases::{PeerHandle, BF};

pub struct InitialCheckResults {
    pub needed_pieces: BF,
    pub have_pieces: BF,
    pub have_bytes: u64,
    pub needed_bytes: u64,
}

pub fn update_hash_from_file<Sha1: ISha1>(
    file: &mut File,
    hash: &mut Sha1,
    buf: &mut [u8],
    mut bytes_to_read: usize,
) -> anyhow::Result<()> {
    let mut read = 0;
    while bytes_to_read > 0 {
        let chunk = std::cmp::min(buf.len(), bytes_to_read);
        file.read_exact(&mut buf[..chunk]).with_context(|| {
            format!(
                "failed reading chunk of size {}, read so far {}",
                chunk, read
            )
        })?;
        bytes_to_read -= chunk;
        read += chunk;
        hash.update(&buf[..chunk]);
    }
    Ok(())
}

pub struct FileOps<'a, Sha1> {
    torrent: &'a TorrentMetaV1Info<ByteString>,
    files: &'a [Arc<Mutex<File>>],
    lengths: &'a Lengths,
    phantom_data: PhantomData<Sha1>,
}

impl<'a, Sha1Impl: ISha1> FileOps<'a, Sha1Impl> {
    pub fn new(
        torrent: &'a TorrentMetaV1Info<ByteString>,
        files: &'a [Arc<Mutex<File>>],
        lengths: &'a Lengths,
    ) -> Self {
        Self {
            torrent,
            files,
            lengths,
            phantom_data: PhantomData,
        }
    }

    pub fn initial_check(
        &self,
        only_files: Option<&[usize]>,
    ) -> anyhow::Result<InitialCheckResults> {
        let mut needed_pieces = BF::from_vec(vec![0u8; self.lengths.piece_bitfield_bytes()]);
        let mut have_pieces = BF::from_vec(vec![0u8; self.lengths.piece_bitfield_bytes()]);

        let mut have_bytes = 0u64;
        let mut needed_bytes = 0u64;

        struct CurrentFile<'a> {
            index: usize,
            fd: &'a Arc<Mutex<File>>,
            len: u64,
            name: FileIteratorName<'a, ByteString>,
            full_file_required: bool,
            processed_bytes: u64,
            is_broken: bool,
        }
        impl<'a> CurrentFile<'a> {
            fn remaining(&self) -> u64 {
                self.len - self.processed_bytes
            }
            fn mark_processed_bytes(&mut self, bytes: u64) {
                self.processed_bytes += bytes as u64
            }
        }
        let mut file_iterator = self
            .files
            .iter()
            .zip(self.torrent.iter_filenames_and_lengths()?)
            .enumerate()
            .map(|(idx, (fd, (name, len)))| {
                let full_file_required = if let Some(only_files) = only_files {
                    only_files.contains(&idx)
                } else {
                    true
                };
                CurrentFile {
                    index: idx,
                    fd,
                    len,
                    name,
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
            let mut computed_hash = Sha1Impl::new();
            let mut piece_remaining = piece_info.len as usize;
            let mut some_files_broken = false;
            let mut at_least_one_file_required = current_file.full_file_required;

            while piece_remaining > 0 {
                let mut to_read_in_file =
                    std::cmp::min(current_file.remaining(), piece_remaining as u64) as usize;
                while to_read_in_file == 0 {
                    current_file = file_iterator
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("broken torrent metadata"))?;

                    at_least_one_file_required |= current_file.full_file_required;

                    to_read_in_file =
                        std::cmp::min(current_file.remaining(), piece_remaining as u64) as usize;
                }

                let pos = current_file.processed_bytes;
                piece_remaining -= to_read_in_file;
                current_file.mark_processed_bytes(to_read_in_file as u64);

                if current_file.is_broken {
                    // no need to read.
                    continue;
                }

                let mut fd = current_file.fd.lock();

                fd.seek(SeekFrom::Start(pos)).unwrap();
                if let Err(err) = update_hash_from_file(
                    &mut fd,
                    &mut computed_hash,
                    &mut read_buffer,
                    to_read_in_file,
                ) {
                    debug!(
                        "error reading from file {} ({:?}) at {}: {:#}",
                        current_file.index, current_file.name, pos, &err
                    );
                    current_file.is_broken = true;
                    some_files_broken = true;
                }
            }

            if at_least_one_file_required && some_files_broken {
                trace!(
                    "piece {} had errors, marking as needed",
                    piece_info.piece_index
                );

                needed_bytes += piece_info.len as u64;
                needed_pieces.set(piece_info.piece_index.get() as usize, true);
                continue;
            }

            if self
                .torrent
                .compare_hash(piece_info.piece_index.get(), computed_hash.finish())
                .unwrap()
            {
                trace!(
                    "piece {} is fine, not marking as needed",
                    piece_info.piece_index
                );
                have_bytes += piece_info.len as u64;
                have_pieces.set(piece_info.piece_index.get() as usize, true);
            } else if at_least_one_file_required {
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
            needed_pieces,
            have_pieces,
            have_bytes,
            needed_bytes,
        })
    }

    pub fn check_piece(
        &self,
        who_sent: PeerHandle,
        piece_index: ValidPieceIndex,
        last_received_chunk: &ChunkInfo,
    ) -> anyhow::Result<bool> {
        let mut h = Sha1Impl::new();
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

            let to_read_in_file =
                std::cmp::min(file_remaining_len, piece_remaining_bytes as u64) as usize;
            let mut file_g = self.files[file_idx].lock();
            debug!(
                "piece={}, handle={}, file_idx={}, seeking to {}. Last received chunk: {:?}",
                piece_index, who_sent, file_idx, absolute_offset, &last_received_chunk
            );
            file_g
                .seek(SeekFrom::Start(absolute_offset))
                .with_context(|| {
                    format!(
                        "error seeking to {}, file id: {}",
                        absolute_offset, file_idx
                    )
                })?;
            update_hash_from_file(&mut file_g, &mut h, &mut buf, to_read_in_file).with_context(
                || {
                    format!(
                        "error reading {} bytes, file_id: {} (\"{:?}\")",
                        to_read_in_file, file_idx, name
                    )
                },
            )?;

            piece_remaining_bytes -= to_read_in_file;

            if piece_remaining_bytes == 0 {
                return Ok(true);
            }

            absolute_offset = 0;
        }

        match self.torrent.compare_hash(piece_index.get(), h.finish()) {
            Some(true) => {
                debug!("piece={} hash matches", piece_index);
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
        let mut absolute_offset = self.lengths.chunk_absolute_offset(&chunk_info);
        let mut buf = result_buf;

        for (file_idx, file_len) in self.torrent.iter_file_lengths()?.enumerate() {
            if absolute_offset > file_len {
                absolute_offset -= file_len;
                continue;
            }
            let file_remaining_len = file_len - absolute_offset;
            let to_read_in_file = std::cmp::min(file_remaining_len, buf.len() as u64) as usize;

            let mut file_g = self.files[file_idx].lock();
            debug!(
                "piece={}, handle={}, file_idx={}, seeking to {}. To read chunk: {:?}",
                chunk_info.piece_index, who_sent, file_idx, absolute_offset, &chunk_info
            );
            file_g
                .seek(SeekFrom::Start(absolute_offset))
                .with_context(|| {
                    format!(
                        "error seeking to {}, file id: {}",
                        absolute_offset, file_idx
                    )
                })?;
            file_g
                .read_exact(&mut buf[..to_read_in_file])
                .with_context(|| {
                    format!(
                        "error reading {} bytes, file_id: {}",
                        file_idx, to_read_in_file
                    )
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
        let mut absolute_offset = self.lengths.chunk_absolute_offset(&chunk_info);

        for (file_idx, (name, file_len)) in self.torrent.iter_filenames_and_lengths()?.enumerate() {
            if absolute_offset > file_len {
                absolute_offset -= file_len;
                continue;
            }

            let remaining_len = file_len - absolute_offset;
            let to_write = std::cmp::min(buf.len(), remaining_len as usize);

            let mut file_g = self.files[file_idx].lock();
            debug!(
                "piece={}, chunk={:?}, handle={}, begin={}, file={}, writing {} bytes at {}",
                chunk_info.piece_index,
                chunk_info,
                who_sent,
                chunk_info.offset,
                file_idx,
                to_write,
                absolute_offset
            );
            file_g
                .seek(SeekFrom::Start(absolute_offset))
                .with_context(|| {
                    format!(
                        "error seeking to {} in file {} (\"{:?}\")",
                        absolute_offset, file_idx, name
                    )
                })?;
            file_g
                .write_all(&buf[..to_write])
                .with_context(|| format!("error writing to file {} (\"{:?}\")", file_idx, name))?;
            buf = &buf[to_write..];
            if buf.is_empty() {
                break;
            }

            absolute_offset = 0;
        }

        Ok(())
    }
}
