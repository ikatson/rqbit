use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    sync::Arc,
};

use anyhow::Context;
use log::{debug, trace};
use parking_lot::Mutex;

use crate::{
    buffers::ByteString,
    lengths::Lengths,
    torrent_metainfo::{FileIteratorName, TorrentMetaV1Owned},
    type_aliases::BF,
};

pub struct InitialCheckResults {
    pub needed_pieces: BF,
    pub have_pieces: BF,
    pub have_bytes: u64,
    pub needed_bytes: u64,
}

pub fn update_hash_from_file(
    file: &mut File,
    hash: &mut sha1::Sha1,
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

pub fn initial_check(
    torrent: &TorrentMetaV1Owned,
    files: &[Arc<Mutex<File>>],
    only_files: Option<&[usize]>,
    lengths: &Lengths,
) -> anyhow::Result<InitialCheckResults> {
    let mut needed_pieces = BF::from_vec(vec![0u8; lengths.piece_bitfield_bytes()]);
    let mut have_pieces = BF::from_vec(vec![0u8; lengths.piece_bitfield_bytes()]);

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
    let mut file_iterator = files
        .iter()
        .zip(torrent.info.iter_filenames_and_lengths())
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

    for piece_info in lengths.iter_piece_infos() {
        let mut computed_hash = sha1::Sha1::new();
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

        if torrent
            .info
            .compare_hash(piece_info.piece_index.get(), &computed_hash)
            .unwrap()
        {
            trace!(
                "piece {} is fine, not marking as needed",
                piece_info.piece_index
            );
            have_bytes += piece_info.len as u64;
            have_pieces.set(piece_info.piece_index.get() as usize, true);
        } else {
            if at_least_one_file_required {
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
    }

    Ok(InitialCheckResults {
        needed_pieces,
        have_pieces,
        have_bytes,
        needed_bytes,
    })
}
