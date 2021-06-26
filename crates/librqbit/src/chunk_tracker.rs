use log::{debug, info};

use crate::{
    buffers::ByteString,
    lengths::{Lengths, ValidPieceIndex},
    peer_comms::Piece,
    type_aliases::BF,
};

pub struct ChunkTracker {
    needed_pieces: BF,
    chunk_status: BF,
    lengths: Lengths,
}

fn compute_chunk_status(lengths: &Lengths, needed_pieces: &BF) -> BF {
    let required_size = lengths.chunk_bitfield_bytes();
    let vec = vec![0u8; required_size];
    let mut chunk_bf = BF::from_vec(vec);
    for piece_index in needed_pieces
        .get(0..lengths.total_pieces() as usize)
        .unwrap()
        .iter_zeros()
    {
        let offset = piece_index * lengths.default_chunks_per_piece() as usize;
        let chunks_per_piece = lengths
            .chunks_per_piece(lengths.validate_piece_index(piece_index as u32).unwrap())
            as usize;
        chunk_bf
            .get_mut(offset..offset + chunks_per_piece)
            .unwrap()
            .set_all(true);
    }
    chunk_bf
}

impl ChunkTracker {
    pub fn new(needed_pieces: BF, lengths: Lengths) -> Self {
        Self {
            chunk_status: compute_chunk_status(&lengths, &needed_pieces),
            needed_pieces,
            lengths,
        }
    }
    pub fn get_needed_pieces(&self) -> &BF {
        &self.needed_pieces
    }
    pub fn reserve_needed_piece(&mut self, index: ValidPieceIndex) {
        self.needed_pieces.set(index.get() as usize, false)
    }
    pub fn mark_piece_needed(&mut self, index: ValidPieceIndex) -> bool {
        info!("remarking piece={} as needed", index);
        self.needed_pieces.set(index.get() as usize, true);
        self.chunk_status
            .get_mut(self.lengths.chunk_range(index))
            .map(|s| {
                s.set_all(false);
                true
            })
            .unwrap_or_default()
    }

    // return true if the whole piece is marked downloaded
    pub fn mark_chunk_downloaded(&mut self, piece: &Piece<ByteString>) -> Option<bool> {
        let chunk_info = self.lengths.chunk_info_from_received_piece(piece)?;
        self.chunk_status
            .set(chunk_info.absolute_index as usize, true);
        let chunk_range = self.lengths.chunk_range(chunk_info.piece_index);
        let chunk_range = self.chunk_status.get(chunk_range).unwrap();
        let all = chunk_range.all();

        debug!(
            "piece={}, chunk_info={:?}, bits={:?}",
            piece.index, chunk_info, chunk_range,
        );
        Some(all)
    }
}
