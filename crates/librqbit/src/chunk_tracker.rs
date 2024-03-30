use std::collections::HashSet;

use anyhow::Context;
use buffers::ByteBufOwned;
use librqbit_core::{
    lengths::{ChunkInfo, Lengths, ValidPieceIndex},
    torrent_metainfo::{TorrentMetaV1Info, TorrentMetaV1Owned},
};
use peer_binary_protocol::Piece;
use tracing::{debug, trace};

use crate::type_aliases::BF;

pub struct ChunkTracker {
    // This forms the basis of a "queue" to pull from.
    // It's set to 1 if we need a piece, but the moment we start requesting a peer,
    // it's set to 0.
    //
    // Initially this is the opposite of "have", until we start making requests.
    // An in-flight request is not in "needed", and not in "have".
    needed_pieces: BF,

    // This has a bit set per each chunk (block) that we have written to the output file.
    // It doesn't mean it's valid yet. Used to track how much is left in each piece.
    chunk_status: BF,

    // These are the pieces that we actually have, fully checked and downloaded.
    have: BF,

    lengths: Lengths,

    // What pieces to download first.
    priority_piece_ids: Vec<usize>,

    total_selected_bytes: u64,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct HaveNeeded {
    pub have_bytes: u64,
    pub needed_bytes: u64,
}

// Comput the have-status of chunks.
//
// Save as "have_pieces", but there's one bit per chunk (not per piece).
fn compute_chunk_have_status(lengths: &Lengths, have_pieces: &BF) -> anyhow::Result<BF> {
    if have_pieces.len() < lengths.total_pieces() as usize {
        anyhow::bail!(
            "bug: have_pieces.len() < lengths.total_pieces(); {} < {}",
            have_pieces.len(),
            lengths.total_pieces()
        );
    }
    let required_size = lengths.chunk_bitfield_bytes();
    let vec = vec![0u8; required_size];
    let mut chunk_bf = BF::from_boxed_slice(vec.into_boxed_slice());

    for piece in lengths.iter_piece_infos() {
        let chunks = lengths.chunks_per_piece(piece.piece_index) as usize;
        let offset = (lengths.default_chunks_per_piece() * piece.piece_index.get()) as usize;
        let range = offset..(offset + chunks);
        if have_pieces[piece.piece_index.get() as usize] {
            chunk_bf
                .get_mut(range.clone())
                .with_context(|| {
                    format!("bug in bitvec: error getting range {range:?} from chunk_bf")
                })?
                .fill(true);
        }
    }
    Ok(chunk_bf)
}

pub enum ChunkMarkingResult {
    PreviouslyCompleted,
    NotCompleted,
    Completed,
}

impl ChunkTracker {
    pub fn new(
        // Needed pieces are the ones we need to download. NOTE: if all files are selected,
        // this is the inverse of have_pieces. But if partial files are selected, we may need more/less
        // than we have.
        needed_pieces: BF,
        // Have pieces are the ones we have already downloaded and verified.
        have_pieces: BF,
        lengths: Lengths,
        total_selected_bytes: u64,
    ) -> anyhow::Result<Self> {
        // TODO: ideally this needs to be a list based on needed files, e.g.
        // last needed piece for each file. But let's keep simple for now.

        // TODO: bitvec is bugged, the short version panics.
        // let last_needed_piece_id = needed_pieces.iter_ones().next_back();
        let last_needed_piece_id = needed_pieces
            .iter()
            .enumerate()
            .filter_map(|(id, b)| if *b { Some(id) } else { None })
            .last();

        // The last pieces first. Often important information is stored in the last piece.
        // E.g. if it's a video file, than the last piece often contains some index, or just
        // players look into it, and it's better be there.
        let priority_piece_ids = last_needed_piece_id.into_iter().collect();
        Ok(Self {
            chunk_status: compute_chunk_have_status(&lengths, &have_pieces)
                .context("error computing chunk status")?,
            needed_pieces,
            lengths,
            have: have_pieces,
            priority_piece_ids,
            total_selected_bytes,
        })
    }

    pub fn get_total_selected_bytes(&self) -> u64 {
        self.total_selected_bytes
    }

    pub fn get_lengths(&self) -> &Lengths {
        &self.lengths
    }

    pub fn get_have_pieces(&self) -> &BF {
        &self.have
    }
    pub fn reserve_needed_piece(&mut self, index: ValidPieceIndex) {
        self.needed_pieces.set(index.get() as usize, false)
    }

    pub fn calc_have_bytes(&self) -> u64 {
        self.have
            .iter_ones()
            .filter_map(|piece_id| {
                let piece_id = self.lengths.validate_piece_index(piece_id as u32)?;
                Some(self.lengths.piece_length(piece_id) as u64)
            })
            .sum()
    }

    pub fn calc_needed_bytes(&self) -> u64 {
        self.needed_pieces
            .iter_ones()
            .filter_map(|piece_id| {
                let piece_id = self.lengths.validate_piece_index(piece_id as u32)?;
                Some(self.lengths.piece_length(piece_id) as u64)
            })
            .sum()
    }

    pub fn iter_needed_pieces(&self) -> impl Iterator<Item = usize> + '_ {
        self.priority_piece_ids
            .iter()
            .copied()
            .filter(move |piece_id| self.needed_pieces[*piece_id])
            .chain(
                self.needed_pieces
                    .iter_ones()
                    .filter(move |id| !self.priority_piece_ids.contains(id)),
            )
    }

    // None if wrong chunk
    // true if did something
    // false if didn't do anything
    pub fn mark_chunk_request_cancelled(
        &mut self,
        index: ValidPieceIndex,
        _chunk: u32,
    ) -> Option<bool> {
        if *self.have.get(index.get() as usize)? {
            return Some(false);
        }
        // This will trigger the requesters to re-check each chunk in this piece.
        let chunk_range = self.lengths.chunk_range(index);
        if !self.chunk_status.get(chunk_range)?.all() {
            self.needed_pieces.set(index.get() as usize, true);
        }
        Some(true)
    }

    pub fn mark_piece_broken_if_not_have(&mut self, index: ValidPieceIndex) {
        if self
            .have
            .get(index.get() as usize)
            .map(|r| *r)
            .unwrap_or_default()
        {
            return;
        }
        debug!("remarking piece={} as broken", index);
        self.needed_pieces.set(index.get() as usize, true);
        if let Some(s) = self.chunk_status.get_mut(self.lengths.chunk_range(index)) {
            s.fill(false);
        }
    }

    pub fn mark_piece_downloaded(&mut self, idx: ValidPieceIndex) {
        self.have.set(idx.get() as usize, true);
    }

    pub fn is_chunk_ready_to_upload(&self, chunk: &ChunkInfo) -> bool {
        self.have
            .get(chunk.piece_index.get() as usize)
            .map(|b| *b)
            .unwrap_or(false)
    }

    // return true if the whole piece is marked downloaded
    pub fn mark_chunk_downloaded<ByteBuf>(
        &mut self,
        piece: &Piece<ByteBuf>,
    ) -> Option<ChunkMarkingResult>
    where
        ByteBuf: AsRef<[u8]>,
    {
        let chunk_info = self.lengths.chunk_info_from_received_data(
            self.lengths.validate_piece_index(piece.index)?,
            piece.begin,
            piece.block.as_ref().len() as u32,
        )?;
        let chunk_range = self.lengths.chunk_range(chunk_info.piece_index);
        let chunk_range = self.chunk_status.get_mut(chunk_range).unwrap();
        if chunk_range.all() {
            return Some(ChunkMarkingResult::PreviouslyCompleted);
        }
        chunk_range.set(chunk_info.chunk_index as usize, true);
        trace!(
            "piece={}, chunk_info={:?}, bits={:?}",
            piece.index,
            chunk_info,
            chunk_range,
        );

        if chunk_range.all() {
            return Some(ChunkMarkingResult::Completed);
        }
        Some(ChunkMarkingResult::NotCompleted)
    }

    // NOTE: this doesn't validate new_only_files.
    // E.g. if there are indices there that don't make
    // sense, they will be ignored.
    pub fn update_only_files(
        &mut self,
        file_lengths_iterator: impl IntoIterator<Item = u64>,
        // TODO: maybe make this a BF
        new_only_files: &HashSet<usize>,
    ) -> anyhow::Result<HaveNeeded> {
        let mut piece_it = self.lengths.iter_piece_infos();
        let mut current_piece = piece_it
            .next()
            .context("bug: iter_piece_infos() returned empty iterator")?;
        let mut current_piece_needed = false;
        let mut current_piece_remaining = current_piece.len;
        let mut have_bytes = 0u64;
        let mut needed_bytes = 0u64;

        for (idx, len) in file_lengths_iterator.into_iter().enumerate() {
            let file_required = new_only_files.contains(&idx);

            let mut remaining_file_len = len;

            while remaining_file_len > 0 {
                current_piece_needed |= len > 0 && file_required;
                let shift = std::cmp::min(current_piece_remaining as u64, remaining_file_len);
                assert!(shift > 0);
                remaining_file_len -= shift;
                current_piece_remaining -= shift as u32;

                dbg!(
                    idx,
                    shift,
                    remaining_file_len,
                    current_piece_remaining,
                    current_piece_needed,
                    file_required,
                    current_piece
                );

                if current_piece_remaining == 0 {
                    let current_piece_have = self.have[current_piece.piece_index.get() as usize];
                    if current_piece_have {
                        have_bytes += current_piece.len as u64;
                    }
                    if current_piece_needed {
                        needed_bytes += current_piece.len as u64;
                    }
                    match (current_piece_needed, current_piece_have) {
                        (true, true) => {}
                        (true, false) => {
                            dbg!(self.mark_piece_broken_if_not_have(current_piece.piece_index))
                        }
                        (false, true) => {}
                        (false, false) => {
                            // don't need the piece, and don't have it - cancel downloading it
                            dbg!(self
                                .needed_pieces
                                .set(current_piece.piece_index.get() as usize, false));
                        }
                    }

                    if current_piece.piece_index != self.lengths.last_piece_id() {
                        current_piece = piece_it.next().context(
                            "bug: iter_piece_infos() pieces ended earlier than expected",
                        )?;
                        current_piece_needed = false;
                        current_piece_remaining = current_piece.len;
                    }
                }
            }
        }

        Ok(HaveNeeded {
            have_bytes,
            needed_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use librqbit_core::{constants::CHUNK_SIZE, lengths::Lengths};

    use crate::{chunk_tracker::HaveNeeded, type_aliases::BF};

    use super::{compute_chunk_have_status, ChunkTracker};

    #[test]
    fn test_compute_chunk_status() {
        // Create the most obnoxious lenghts, and ensure it doesn't break in that case.
        let piece_length = CHUNK_SIZE * 2 + 1;
        let l = Lengths::new(piece_length as u64 * 2 + 1, piece_length).unwrap();

        assert_eq!(l.total_pieces(), 3);
        assert_eq!(l.default_chunks_per_piece(), 3);
        assert_eq!(l.total_chunks(), 7);

        {
            let mut have_pieces =
                BF::from_boxed_slice(vec![u8::MAX; l.piece_bitfield_bytes()].into_boxed_slice());
            have_pieces.set(0, false);

            let chunks = compute_chunk_have_status(&l, &have_pieces).unwrap();
            assert_eq!(chunks[0], false);
            assert_eq!(chunks[1], false);
            assert_eq!(chunks[2], false);
            assert_eq!(chunks[3], true);
            assert_eq!(chunks[4], true);
            assert_eq!(chunks[5], true);
            assert_eq!(chunks[6], true);
        }

        {
            let mut have_pieces =
                BF::from_boxed_slice(vec![u8::MAX; l.piece_bitfield_bytes()].into_boxed_slice());
            have_pieces.set(1, false);

            let chunks = compute_chunk_have_status(&l, &have_pieces).unwrap();
            dbg!(&chunks);
            assert_eq!(chunks[0], true);
            assert_eq!(chunks[1], true);
            assert_eq!(chunks[2], true);
            assert_eq!(chunks[3], false);
            assert_eq!(chunks[4], false);
            assert_eq!(chunks[5], false);
            assert_eq!(chunks[6], true);
        }

        {
            let mut have_pieces =
                BF::from_boxed_slice(vec![u8::MAX; l.piece_bitfield_bytes()].into_boxed_slice());
            have_pieces.set(2, false);

            let chunks = compute_chunk_have_status(&l, &have_pieces).unwrap();
            dbg!(&chunks);
            assert_eq!(chunks[0], true);
            assert_eq!(chunks[1], true);
            assert_eq!(chunks[2], true);
            assert_eq!(chunks[3], true);
            assert_eq!(chunks[4], true);
            assert_eq!(chunks[5], true);
            assert_eq!(chunks[6], false);
        }

        {
            // A more reasonable case.
            let piece_length = CHUNK_SIZE * 2;
            let l = Lengths::new(piece_length as u64 * 2 + 1, piece_length).unwrap();

            assert_eq!(l.total_pieces(), 3);
            assert_eq!(l.default_chunks_per_piece(), 2);
            assert_eq!(l.total_chunks(), 5);

            {
                let mut have_pieces = BF::from_boxed_slice(
                    vec![u8::MAX; l.piece_bitfield_bytes()].into_boxed_slice(),
                );
                have_pieces.set(1, false);

                let chunks = compute_chunk_have_status(&l, &have_pieces).unwrap();
                dbg!(&chunks);
                assert_eq!(chunks[0], true);
                assert_eq!(chunks[1], true);
                assert_eq!(chunks[2], false);
                assert_eq!(chunks[3], false);
                assert_eq!(chunks[4], true);
            }

            {
                let mut have_pieces = BF::from_boxed_slice(
                    vec![u8::MAX; l.piece_bitfield_bytes()].into_boxed_slice(),
                );
                have_pieces.set(2, false);

                let chunks = compute_chunk_have_status(&l, &have_pieces).unwrap();
                dbg!(&chunks);
                assert_eq!(chunks[0], true);
                assert_eq!(chunks[1], true);
                assert_eq!(chunks[2], true);
                assert_eq!(chunks[3], true);
                assert_eq!(chunks[4], false);
            }
        }
    }

    #[test]
    fn test_update_only_files() {
        let piece_len = CHUNK_SIZE * 2 + 1;
        let total_len = piece_len as u64 * 2 + 1;
        let l = Lengths::new(total_len, piece_len).unwrap();
        assert_eq!(l.total_pieces(), 3);
        assert_eq!(l.total_chunks(), 7);

        let all_files = [
            piece_len as u64, // piece 0 and boundary
            1,                // piece 1
            0,                // piece 1 (or none)
            piece_len as u64, // piece 1 and 2
        ];

        let bf_len = l.piece_bitfield_bytes();
        let initial_have = BF::from_boxed_slice(vec![0u8; bf_len].into_boxed_slice());
        let initial_needed = BF::from_boxed_slice(vec![u8::MAX; bf_len].into_boxed_slice());

        // Initially, we need all files and all pieces.
        let mut ct = ChunkTracker::new(
            initial_needed.clone(),
            initial_have.clone(),
            l,
            l.total_length(),
        )
        .unwrap();

        // Select all file, no changes.
        assert_eq!(
            ct.update_only_files(all_files.into_iter(), &HashSet::from_iter([0, 1, 2, 3]))
                .unwrap(),
            HaveNeeded {
                have_bytes: 0,
                needed_bytes: total_len
            }
        );
        assert_eq!(ct.have, initial_have);
        assert_eq!(ct.needed_pieces, initial_needed);

        // Select only the first file.
        println!("Select only the first file.");
        assert_eq!(
            ct.update_only_files(all_files, &HashSet::from_iter([0]))
                .unwrap(),
            HaveNeeded {
                have_bytes: 0,
                needed_bytes: all_files[0],
            }
        );
        assert_eq!(ct.needed_pieces[0], true);
        assert_eq!(ct.needed_pieces[1], false);
        assert_eq!(ct.needed_pieces[2], false);

        // Select only the second file.
        assert_eq!(
            ct.update_only_files(all_files, &HashSet::from_iter([1]))
                .unwrap(),
            HaveNeeded {
                have_bytes: 0,
                needed_bytes: piece_len as u64,
            }
        );
        assert_eq!(ct.needed_pieces[0], false);
        assert_eq!(ct.needed_pieces[1], true);
        assert_eq!(ct.needed_pieces[2], false);

        // Select only the third file (zero sized one!).
        assert_eq!(
            ct.update_only_files(all_files, &HashSet::from_iter([2]))
                .unwrap(),
            HaveNeeded {
                have_bytes: 0,
                needed_bytes: 0,
            }
        );
        assert_eq!(ct.needed_pieces[0], false);
        assert_eq!(ct.needed_pieces[1], false);
        assert_eq!(ct.needed_pieces[2], false);

        // Select only the fourth file.
        assert_eq!(
            ct.update_only_files(all_files, &HashSet::from_iter([3]))
                .unwrap(),
            HaveNeeded {
                have_bytes: 0,
                needed_bytes: (piece_len + 1) as u64,
            }
        );
        assert_eq!(ct.needed_pieces[0], false);
        assert_eq!(ct.needed_pieces[1], true);
        assert_eq!(ct.needed_pieces[2], true);

        // Select first and last file
        assert_eq!(
            ct.update_only_files(all_files.clone(), &HashSet::from_iter([0, 3]))
                .unwrap(),
            HaveNeeded {
                have_bytes: 0,
                needed_bytes: all_files[0] + all_files[3] + 1,
            }
        );
        assert_eq!(ct.needed_pieces[0], true);
        assert_eq!(ct.needed_pieces[1], true);
        assert_eq!(ct.needed_pieces[2], true);

        // Select all files
        assert_eq!(
            ct.update_only_files(all_files.clone(), &HashSet::from_iter([0, 1, 2, 3]))
                .unwrap(),
            HaveNeeded {
                have_bytes: 0,
                needed_bytes: total_len,
            }
        );
        assert_eq!(ct.needed_pieces[0], true);
        assert_eq!(ct.needed_pieces[1], true);
        assert_eq!(ct.needed_pieces[2], true);
    }
}
