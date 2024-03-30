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

// TODO: this should be redone from "have" pieces, not from "needed" pieces.
// Needed pieces are the ones we need to download, not necessarily the ones we have.
// E.g. we might have more pieces, but the client asks to download only some files
// partially.
fn compute_chunk_status(lengths: &Lengths, needed_pieces: &BF) -> anyhow::Result<BF> {
    if needed_pieces.len() < lengths.total_pieces() as usize {
        anyhow::bail!(
            "bug: needed_pieces.len() < lengths.total_pieces(); {} < {}",
            needed_pieces.len(),
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
        if !needed_pieces[piece.piece_index.get() as usize] {
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
        needed_pieces: BF,
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
            chunk_status: compute_chunk_status(&lengths, &needed_pieces)
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
}

#[cfg(test)]
mod tests {
    use librqbit_core::{constants::CHUNK_SIZE, lengths::Lengths};

    use crate::type_aliases::BF;

    use super::compute_chunk_status;

    #[test]
    fn test_compute_chunk_status() {
        // Create the most obnoxious lenghts, and ensure it doesn't break in that case.
        let piece_length = CHUNK_SIZE * 2 + 1;
        let l = Lengths::new(piece_length as u64 * 2 + 1, piece_length).unwrap();

        assert_eq!(l.total_pieces(), 3);
        assert_eq!(l.default_chunks_per_piece(), 3);
        assert_eq!(l.total_chunks(), 7);

        {
            let mut needed_pieces =
                BF::from_boxed_slice(vec![0u8; l.piece_bitfield_bytes()].into_boxed_slice());
            needed_pieces.set(0, true);

            let chunks = compute_chunk_status(&l, &needed_pieces).unwrap();
            assert_eq!(chunks[0], false);
            assert_eq!(chunks[1], false);
            assert_eq!(chunks[2], false);
            assert_eq!(chunks[3], true);
            assert_eq!(chunks[4], true);
            assert_eq!(chunks[5], true);
            assert_eq!(chunks[6], true);
        }

        {
            let mut needed_pieces =
                BF::from_boxed_slice(vec![0u8; l.piece_bitfield_bytes()].into_boxed_slice());
            needed_pieces.set(1, true);

            let chunks = compute_chunk_status(&l, &needed_pieces).unwrap();
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
            let mut needed_pieces =
                BF::from_boxed_slice(vec![0u8; l.piece_bitfield_bytes()].into_boxed_slice());
            needed_pieces.set(2, true);

            let chunks = compute_chunk_status(&l, &needed_pieces).unwrap();
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
                let mut needed_pieces =
                    BF::from_boxed_slice(vec![0u8; l.piece_bitfield_bytes()].into_boxed_slice());
                needed_pieces.set(1, true);

                let chunks = compute_chunk_status(&l, &needed_pieces).unwrap();
                dbg!(&chunks);
                assert_eq!(chunks[0], true);
                assert_eq!(chunks[1], true);
                assert_eq!(chunks[2], false);
                assert_eq!(chunks[3], false);
                assert_eq!(chunks[4], true);
            }

            {
                let mut needed_pieces =
                    BF::from_boxed_slice(vec![0u8; l.piece_bitfield_bytes()].into_boxed_slice());
                needed_pieces.set(2, true);

                let chunks = compute_chunk_status(&l, &needed_pieces).unwrap();
                dbg!(&chunks);
                assert_eq!(chunks[0], true);
                assert_eq!(chunks[1], true);
                assert_eq!(chunks[2], true);
                assert_eq!(chunks[3], true);
                assert_eq!(chunks[4], false);
            }
        }
    }
}
