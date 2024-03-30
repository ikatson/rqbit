use std::collections::HashSet;

use anyhow::Context;
use librqbit_core::lengths::{ChunkInfo, Lengths, ValidPieceIndex};
use peer_binary_protocol::Piece;
use tracing::{debug, trace};

use crate::type_aliases::BF;

pub struct ChunkTracker {
    // This forms the basis of a "queue" to pull from.
    // It's set to 1 if we need a piece, but the moment we start requesting a peer,
    // it's set to 0.
    //
    // Initially this is the opposite of "have", until we start making requests.
    // An in-flight request is not in in the queue, and not in "have".
    //
    // needed initial value = selected & !have
    queue_pieces: BF,

    // This has a bit set per each chunk (block) that we have written to the output file.
    // It doesn't mean it's valid yet. Used to track how much is left in each piece.
    chunk_status: BF,

    // These are the pieces that we actually have, fully checked and downloaded.
    have: BF,

    // The pieces that the user selected. This doesn't change unless update_only_files
    // was called.
    selected: BF,

    lengths: Lengths,

    // What pieces to download first.
    priority_piece_ids: Vec<usize>,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct HaveNeededSelected {
    // How many bytes we have downloaded and verified.
    pub have_bytes: u64,
    // How many bytes do we need to download for selected to be
    // a subset of have.
    pub needed_bytes: u64,
    // How many bytes the user selected (by picking files).
    pub selected_bytes: u64,
}

impl HaveNeededSelected {
    pub const fn progress(&self) -> u64 {
        self.selected_bytes - self.needed_bytes
    }

    pub const fn total(&self) -> u64 {
        self.selected_bytes
    }

    pub const fn finished(&self) -> bool {
        self.needed_bytes == 0
    }
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

fn compute_queued_pieces_unchecked(have_pieces: &BF, selected_pieces: &BF) -> BF {
    // it's needed ONLY if it's selected and we don't have it.
    use core::ops::BitAnd;
    use core::ops::Not;

    have_pieces.clone().not().bitand(selected_pieces)
}

fn compute_queued_pieces(have_pieces: &BF, selected_pieces: &BF) -> anyhow::Result<BF> {
    if have_pieces.len() != selected_pieces.len() {
        anyhow::bail!(
            "have_pieces.len() != selected_pieces.len(), {} != {}",
            have_pieces.len(),
            selected_pieces.len()
        );
    }

    Ok(compute_queued_pieces_unchecked(
        have_pieces,
        selected_pieces,
    ))
}

pub enum ChunkMarkingResult {
    PreviouslyCompleted,
    NotCompleted,
    Completed,
}

impl ChunkTracker {
    pub fn new(
        // Have pieces are the ones we have already downloaded and verified.
        have_pieces: BF,
        // Selected pieces are the ones the user has selected
        selected_pieces: BF,
        lengths: Lengths,
    ) -> anyhow::Result<Self> {
        let needed_pieces = compute_queued_pieces(&have_pieces, &selected_pieces)
            .context("error computing needed pieces")?;

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
            queue_pieces: needed_pieces,
            selected: selected_pieces,
            lengths,
            have: have_pieces,
            priority_piece_ids,
        })
    }

    pub fn get_lengths(&self) -> &Lengths {
        &self.lengths
    }

    pub fn get_have_pieces(&self) -> &BF {
        &self.have
    }
    pub fn reserve_needed_piece(&mut self, index: ValidPieceIndex) {
        self.queue_pieces.set(index.get() as usize, false)
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
        self.have
            .iter()
            .zip(self.selected.iter())
            .enumerate()
            .filter_map(|(piece_id, (have, selected))| {
                if *selected && !*have {
                    let piece_id = self.lengths.validate_piece_index(piece_id as u32)?;
                    Some(self.lengths.piece_length(piece_id) as u64)
                } else {
                    None
                }
            })
            .sum()
    }

    pub fn iter_queued_pieces(&self) -> impl Iterator<Item = usize> + '_ {
        self.priority_piece_ids
            .iter()
            .copied()
            .filter(move |piece_id| self.queue_pieces[*piece_id])
            .chain(
                self.queue_pieces
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
            self.queue_pieces.set(index.get() as usize, true);
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
        self.queue_pieces.set(index.get() as usize, true);
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
    ) -> anyhow::Result<HaveNeededSelected> {
        let mut piece_it = self.lengths.iter_piece_infos();
        let mut current_piece = piece_it
            .next()
            .context("bug: iter_piece_infos() returned empty iterator")?;
        let mut current_piece_selected = false;
        let mut current_piece_remaining = current_piece.len;
        let mut have_bytes = 0u64;
        let mut selected_bytes = 0u64;
        let mut needed_bytes = 0u64;

        for (idx, len) in file_lengths_iterator.into_iter().enumerate() {
            let file_required = new_only_files.contains(&idx);

            let mut remaining_file_len = len;

            while remaining_file_len > 0 {
                current_piece_selected |= len > 0 && file_required;
                let shift = std::cmp::min(current_piece_remaining as u64, remaining_file_len);
                if shift == 0 {
                    anyhow::bail!("bug: shift = 0, this shouldn't have happened")
                }
                remaining_file_len -= shift;
                current_piece_remaining -= shift as u32;

                if current_piece_remaining == 0 {
                    let current_piece_have = self.have[current_piece.piece_index.get() as usize];
                    if current_piece_have {
                        have_bytes += current_piece.len as u64;
                    }
                    if current_piece_selected {
                        selected_bytes += current_piece.len as u64;
                    }
                    if current_piece_selected && !current_piece_have {
                        needed_bytes += current_piece.len as u64;
                    }
                    self.selected.set(
                        current_piece.piece_index.get() as usize,
                        current_piece_selected,
                    );
                    match (current_piece_selected, current_piece_have) {
                        (true, true) => {}
                        (true, false) => {
                            self.mark_piece_broken_if_not_have(current_piece.piece_index)
                        }
                        (false, true) => {}
                        (false, false) => {
                            // don't need the piece, and don't have it - cancel downloading it
                            self.queue_pieces
                                .set(current_piece.piece_index.get() as usize, false);
                        }
                    }

                    if current_piece.piece_index != self.lengths.last_piece_id() {
                        current_piece = piece_it.next().context(
                            "bug: iter_piece_infos() pieces ended earlier than expected",
                        )?;
                        current_piece_selected = false;
                        current_piece_remaining = current_piece.len;
                    }
                }
            }
        }

        Ok(HaveNeededSelected {
            have_bytes,
            needed_bytes,
            selected_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use librqbit_core::{constants::CHUNK_SIZE, lengths::Lengths};

    use crate::{chunk_tracker::HaveNeededSelected, type_aliases::BF};

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
        let initial_selected = BF::from_boxed_slice(vec![u8::MAX; bf_len].into_boxed_slice());

        // Initially, we need all files and all pieces.
        let mut ct = ChunkTracker::new(initial_have.clone(), initial_selected.clone(), l).unwrap();

        // Select all file, no changes.
        assert_eq!(
            ct.update_only_files(all_files.into_iter(), &HashSet::from_iter([0, 1, 2, 3]))
                .unwrap(),
            HaveNeededSelected {
                have_bytes: 0,
                selected_bytes: total_len,
                needed_bytes: total_len,
            }
        );
        assert_eq!(ct.have, initial_have);
        assert_eq!(ct.queue_pieces, initial_selected);

        // Select only the first file.
        println!("Select only the first file.");
        assert_eq!(
            ct.update_only_files(all_files, &HashSet::from_iter([0]))
                .unwrap(),
            HaveNeededSelected {
                have_bytes: 0,
                selected_bytes: all_files[0],
                needed_bytes: all_files[0],
            }
        );
        assert_eq!(ct.queue_pieces[0], true);
        assert_eq!(ct.queue_pieces[1], false);
        assert_eq!(ct.queue_pieces[2], false);

        // Select only the second file.
        assert_eq!(
            ct.update_only_files(all_files, &HashSet::from_iter([1]))
                .unwrap(),
            HaveNeededSelected {
                have_bytes: 0,
                selected_bytes: piece_len as u64,
                needed_bytes: piece_len as u64,
            }
        );
        assert_eq!(ct.queue_pieces[0], false);
        assert_eq!(ct.queue_pieces[1], true);
        assert_eq!(ct.queue_pieces[2], false);

        // Select only the third file (zero sized one!).
        assert_eq!(
            ct.update_only_files(all_files, &HashSet::from_iter([2]))
                .unwrap(),
            HaveNeededSelected {
                have_bytes: 0,
                selected_bytes: 0,
                needed_bytes: 0,
            }
        );
        assert_eq!(ct.queue_pieces[0], false);
        assert_eq!(ct.queue_pieces[1], false);
        assert_eq!(ct.queue_pieces[2], false);

        // Select only the fourth file.
        assert_eq!(
            ct.update_only_files(all_files, &HashSet::from_iter([3]))
                .unwrap(),
            HaveNeededSelected {
                have_bytes: 0,
                selected_bytes: (piece_len + 1) as u64,
                needed_bytes: (piece_len + 1) as u64,
            }
        );
        assert_eq!(ct.queue_pieces[0], false);
        assert_eq!(ct.queue_pieces[1], true);
        assert_eq!(ct.queue_pieces[2], true);

        // Select first and last file
        assert_eq!(
            ct.update_only_files(all_files.clone(), &HashSet::from_iter([0, 3]))
                .unwrap(),
            HaveNeededSelected {
                have_bytes: 0,
                selected_bytes: all_files[0] + all_files[3] + 1,
                needed_bytes: all_files[0] + all_files[3] + 1,
            }
        );
        assert_eq!(ct.queue_pieces[0], true);
        assert_eq!(ct.queue_pieces[1], true);
        assert_eq!(ct.queue_pieces[2], true);

        // Select all files
        assert_eq!(
            ct.update_only_files(all_files.clone(), &HashSet::from_iter([0, 1, 2, 3]))
                .unwrap(),
            HaveNeededSelected {
                have_bytes: 0,
                selected_bytes: total_len,
                needed_bytes: total_len
            }
        );
        assert_eq!(ct.queue_pieces[0], true);
        assert_eq!(ct.queue_pieces[1], true);
        assert_eq!(ct.queue_pieces[2], true);
    }
}
