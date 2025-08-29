use std::collections::HashSet;

use anyhow::Context;
use buffers::ByteBuf;
use librqbit_core::lengths::{ChunkInfo, Lengths, ValidPieceIndex};
use peer_binary_protocol::Piece;
use tracing::{debug, trace};

use crate::{
    bitv::{BitV, BoxBitV},
    file_info::FileInfo,
    type_aliases::{BF, BS, FileInfos, FilePriorities},
};

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
    have: BoxBitV,

    // The pieces that the user selected. This doesn't change unless update_only_files
    // was called.
    selected: BF,

    // How many bytes do we have per each file.
    per_file_bytes: Vec<u64>,

    lengths: Lengths,

    // Quick to retrieve stats, that MUST be in sync with the BFs
    // above (have/selected).
    hns: HaveNeededSelected,
}

#[derive(Default, Debug, PartialEq, Eq, Clone, Copy)]
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

// Compute the have-status of chunks.
//
// Save as "have_pieces", but there's one bit per chunk (not per piece).
fn compute_chunk_have_status(lengths: &Lengths, have_pieces: &BS) -> anyhow::Result<BF> {
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

fn compute_queued_pieces_unchecked(have_pieces: &BS, selected_pieces: &BS) -> BF {
    // it's needed ONLY if it's selected and we don't have it.
    use core::ops::BitAnd;
    use core::ops::Not;

    have_pieces
        .to_bitvec()
        .not()
        .bitand(selected_pieces)
        .into_boxed_bitslice()
}

fn compute_queued_pieces(have_pieces: &BS, selected_pieces: &BS) -> anyhow::Result<BF> {
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

pub(crate) fn compute_selected_pieces(
    lengths: &Lengths,
    only_files_is_empty_or_contains: impl Fn(usize) -> bool,
    file_infos: &FileInfos,
) -> BF {
    let mut bf = BF::from_boxed_slice(vec![0u8; lengths.piece_bitfield_bytes()].into_boxed_slice());
    for (_, fi) in file_infos
        .iter()
        .enumerate()
        .filter(|(_, fi)| !fi.attrs.padding)
        .filter(|(id, _)| only_files_is_empty_or_contains(*id))
    {
        if let Some(r) = bf.get_mut(fi.piece_range_usize()) {
            r.fill(true);
        }
    }
    bf
}

#[derive(Debug)]
pub enum ChunkMarkingResult {
    PreviouslyCompleted,
    NotCompleted,
    Completed,
}

impl ChunkTracker {
    pub fn new(
        // Have pieces are the ones we have already downloaded and verified.
        have_pieces: BoxBitV,
        // Selected pieces are the ones the user has selected
        selected_pieces: BF,
        lengths: Lengths,
        file_infos: &FileInfos,
    ) -> anyhow::Result<Self> {
        let needed_pieces = compute_queued_pieces(have_pieces.as_slice(), &selected_pieces)
            .context("error computing needed pieces")?;

        // TODO: ideally this needs to be a list based on needed files, e.g.
        // last needed piece for each file. But let's keep simple for now.

        let mut ct = Self {
            chunk_status: compute_chunk_have_status(&lengths, have_pieces.as_slice())
                .context("error computing chunk status")?,
            queue_pieces: needed_pieces,
            selected: selected_pieces,
            lengths,
            have: have_pieces,
            hns: HaveNeededSelected::default(),
            per_file_bytes: vec![0; file_infos.len()],
        };
        ct.recalculate_per_file_bytes(file_infos);
        ct.hns = ct.calc_hns();
        Ok(ct)
    }

    fn recalculate_per_file_bytes(&mut self, file_infos: &FileInfos) {
        for (slot, fi) in self.per_file_bytes.iter_mut().zip(file_infos.iter()) {
            *slot = fi
                .piece_range
                .clone()
                .filter(|p| self.have.as_slice()[*p as usize])
                .map(|id| {
                    self.lengths
                        .size_of_piece_in_file(id, fi.offset_in_torrent, fi.len)
                })
                .sum();
        }
    }

    pub fn get_lengths(&self) -> &Lengths {
        &self.lengths
    }

    pub fn get_have_pieces(&self) -> &dyn BitV {
        &*self.have
    }

    pub fn get_have_pieces_mut(&mut self) -> &mut dyn BitV {
        &mut *self.have
    }

    pub fn reserve_needed_piece(&mut self, index: ValidPieceIndex) {
        self.queue_pieces.set(index.get() as usize, false)
    }

    pub fn get_hns(&self) -> &HaveNeededSelected {
        &self.hns
    }

    fn calc_hns(&self) -> HaveNeededSelected {
        let mut hns = HaveNeededSelected::default();
        for piece in self.lengths.iter_piece_infos() {
            let id = piece.piece_index.get() as usize;
            let len = piece.len as u64;
            let is_have = self.have.as_slice()[id];
            let is_selected = self.selected[id];
            let is_needed = is_selected && !is_have;
            hns.have_bytes += len * (is_have as u64);
            hns.selected_bytes += len * (is_selected as u64);
            hns.needed_bytes += len * (is_needed as u64);
        }
        hns
    }

    pub(crate) fn iter_queued_pieces<'a>(
        &'a self,
        file_priorities: &'a FilePriorities,
        file_infos: &'a FileInfos,
    ) -> impl Iterator<Item = ValidPieceIndex> + 'a {
        file_priorities
            .iter()
            .filter_map(|p| Some((*p, file_infos.get(*p)?)))
            .filter(|(id, f)| self.per_file_bytes[*id] != f.len)
            .flat_map(|(_id, f)| f.iter_piece_priorities())
            .filter(|id| self.queue_pieces[*id])
            .filter_map(|id| id.try_into().ok())
            .filter_map(|id| self.lengths.validate_piece_index(id))
    }

    pub(crate) fn is_piece_have(&self, id: ValidPieceIndex) -> bool {
        self.have.as_slice()[id.get() as usize]
    }

    pub fn mark_piece_broken_if_not_have(&mut self, index: ValidPieceIndex) {
        if self
            .have
            .as_slice()
            .get(index.get() as usize)
            .map(|r| *r)
            .unwrap_or_default()
        {
            return;
        }
        debug!("marking piece={} as broken", index);
        self.queue_pieces.set(index.get() as usize, true);
        if let Some(s) = self.chunk_status.get_mut(self.lengths.chunk_range(index)) {
            s.fill(false);
        }
    }

    pub fn mark_piece_downloaded(&mut self, idx: ValidPieceIndex) {
        let id = idx.get() as usize;
        if !self.have.as_slice()[id] {
            self.have.as_slice_mut().set(id, true);
            let len = self.lengths.piece_length(idx) as u64;
            self.hns.have_bytes += len;
            if self.selected[id] {
                self.hns.needed_bytes -= len;
            }
        }
    }

    pub fn is_chunk_ready_to_upload(&self, chunk: &ChunkInfo) -> bool {
        self.have
            .as_slice()
            .get(chunk.piece_index.get() as usize)
            .map(|b| *b)
            .unwrap_or(false)
    }

    pub fn get_remaining_bytes(&self) -> u64 {
        self.hns.needed_bytes
    }

    // return true if the whole piece is marked downloaded
    pub fn mark_chunk_downloaded(
        &mut self,
        piece: &Piece<ByteBuf<'_>>,
    ) -> Option<ChunkMarkingResult> {
        let chunk_info = self.lengths.chunk_info_from_received_data(
            self.lengths.validate_piece_index(piece.index)?,
            piece.begin,
            piece.len().try_into().unwrap(),
        )?;
        let chunk_range = self.lengths.chunk_range(chunk_info.piece_index);
        let chunk_range = self.chunk_status.get_mut(chunk_range).unwrap();
        if chunk_range.all() {
            return Some(ChunkMarkingResult::PreviouslyCompleted);
        }
        chunk_range.set(chunk_info.chunk_index as usize, true);
        trace!(
            "piece={}, chunk_info={:?}, bits={:?}",
            piece.index, chunk_info, chunk_range,
        );

        if chunk_range.all() {
            return Some(ChunkMarkingResult::Completed);
        }
        Some(ChunkMarkingResult::NotCompleted)
    }

    pub fn update_only_files(
        &mut self,
        file_infos: &FileInfos,
        new_only_files: &HashSet<usize>,
    ) -> anyhow::Result<HaveNeededSelected> {
        let selected = compute_selected_pieces(
            &self.lengths,
            |idx| new_only_files.contains(&idx),
            file_infos,
        );
        let prev_selected = std::mem::replace(&mut self.selected, selected);

        // prev_selected=false and selected=true and have=false: requeue the piece
        {
            let mut b = BF::from_boxed_slice(
                vec![0u8; self.lengths.piece_bitfield_bytes()].into_boxed_slice(),
            );
            for idx in self
                .selected
                .iter_ones()
                .filter(|idx| !prev_selected[*idx] && !self.have.as_slice()[*idx])
            {
                b.set(idx, true);
            }

            for idx in b.iter_ones() {
                #[allow(clippy::cast_possible_truncation)]
                if let Some(idx) = self.lengths.validate_piece_index(idx as u32) {
                    self.mark_piece_broken_if_not_have(idx);
                }
            }
        }

        // selected=false, have=false: don't need the piece, and don't have it - cancel downloading it
        {
            // TODO: is there a better way to write this?
            // self.queue_pieces &= self.have | self.selected;
            let mut have_or_selected: BF = self.selected.clone();
            have_or_selected |= self.have.as_slice();
            self.queue_pieces &= have_or_selected;
        }

        self.hns = self.calc_hns();
        Ok(self.hns)
    }

    pub(crate) fn get_selected_pieces(&self) -> &BF {
        &self.selected
    }

    pub fn is_file_finished(&self, file_info: &FileInfo) -> bool {
        self.have
            .as_slice()
            .get(file_info.piece_range_usize())
            .map(|r| r.all())
            .unwrap_or(true)
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.get_hns().finished()
    }

    pub fn per_file_have_bytes(&self) -> &[u64] {
        &self.per_file_bytes
    }

    // Returns remaining bytes
    pub fn update_file_have_on_piece_completed(
        &mut self,
        piece_id: ValidPieceIndex,
        file_id: usize,
        file_info: &FileInfo,
    ) -> u64 {
        let diff_have = self.lengths.size_of_piece_in_file(
            piece_id.get(),
            file_info.offset_in_torrent,
            file_info.len,
        );
        self.per_file_bytes[file_id] += diff_have;
        file_info.len.saturating_sub(self.per_file_bytes[file_id])
    }
}

#[cfg(test)]
mod tests {
    use librqbit_core::{constants::CHUNK_SIZE, lengths::Lengths};
    use std::collections::HashSet;

    use crate::{
        bitv::BitV, chunk_tracker::HaveNeededSelected, file_info::FileInfo, type_aliases::BF,
    };

    use super::{ChunkTracker, compute_chunk_have_status};

    #[test]
    fn test_compute_chunk_status() {
        // Create the most obnoxious lengths, and ensure it doesn't break in that case.
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
            assert!(!chunks[0]);
            assert!(!chunks[1]);
            assert!(!chunks[2]);
            assert!(chunks[3]);
            assert!(chunks[4]);
            assert!(chunks[5]);
            assert!(chunks[6]);
        }

        {
            let mut have_pieces =
                BF::from_boxed_slice(vec![u8::MAX; l.piece_bitfield_bytes()].into_boxed_slice());
            have_pieces.set(1, false);

            let chunks = compute_chunk_have_status(&l, &have_pieces).unwrap();
            dbg!(&chunks);
            assert!(chunks[0]);
            assert!(chunks[1]);
            assert!(chunks[2]);
            assert!(!chunks[3]);
            assert!(!chunks[4]);
            assert!(!chunks[5]);
            assert!(chunks[6]);
        }

        {
            let mut have_pieces =
                BF::from_boxed_slice(vec![u8::MAX; l.piece_bitfield_bytes()].into_boxed_slice());
            have_pieces.set(2, false);

            let chunks = compute_chunk_have_status(&l, &have_pieces).unwrap();
            dbg!(&chunks);
            assert!(chunks[0]);
            assert!(chunks[1]);
            assert!(chunks[2]);
            assert!(chunks[3]);
            assert!(chunks[4]);
            assert!(chunks[5]);
            assert!(!chunks[6]);
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
                assert!(chunks[0]);
                assert!(chunks[1]);
                assert!(!chunks[2]);
                assert!(!chunks[3]);
                assert!(chunks[4]);
            }

            {
                let mut have_pieces = BF::from_boxed_slice(
                    vec![u8::MAX; l.piece_bitfield_bytes()].into_boxed_slice(),
                );
                have_pieces.set(2, false);

                let chunks = compute_chunk_have_status(&l, &have_pieces).unwrap();
                dbg!(&chunks);
                assert!(chunks[0]);
                assert!(chunks[1]);
                assert!(chunks[2]);
                assert!(chunks[3]);
                assert!(!chunks[4]);
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

        let all_files = vec![
            FileInfo {
                relative_filename: "0".into(),
                offset_in_torrent: 0,
                piece_range: 0..1,
                len: piece_len as u64,
                attrs: Default::default(),
            },
            FileInfo {
                relative_filename: "1".into(),
                offset_in_torrent: piece_len as u64,
                piece_range: 1..2,
                len: 1,
                attrs: Default::default(),
            },
            FileInfo {
                relative_filename: "2".into(),
                offset_in_torrent: piece_len as u64 + 1,
                piece_range: 1..1,
                len: 0,
                attrs: Default::default(),
            },
            FileInfo {
                relative_filename: "3".into(),
                offset_in_torrent: piece_len as u64 + 1,
                piece_range: 1..3,
                len: piece_len as u64,
                attrs: Default::default(),
            },
        ];

        let bf_len = l.piece_bitfield_bytes();
        let initial_have = BF::from_boxed_slice(vec![0u8; bf_len].into_boxed_slice());
        let initial_selected = {
            let mut bf = BF::from_boxed_slice(vec![0u8; bf_len].into_boxed_slice());
            bf.get_mut(0..3).unwrap().fill(true);
            bf
        };

        // Initially, we need all files and all pieces.
        let mut ct = ChunkTracker::new(
            initial_have.clone().into_dyn(),
            initial_selected.clone(),
            l,
            &Default::default(),
        )
        .unwrap();

        // Select all file, no changes.
        assert_eq!(
            ct.update_only_files(&all_files, &HashSet::from_iter([0, 1, 2, 3]))
                .unwrap(),
            HaveNeededSelected {
                have_bytes: 0,
                selected_bytes: total_len,
                needed_bytes: total_len,
            }
        );
        assert_eq!(ct.have.as_slice(), initial_have.as_bitslice());
        assert_eq!(ct.queue_pieces, initial_selected);

        // Select only the first file.
        println!("Select only the first file.");
        assert_eq!(
            ct.update_only_files(&all_files, &HashSet::from_iter([0]))
                .unwrap(),
            HaveNeededSelected {
                have_bytes: 0,
                selected_bytes: all_files[0].len,
                needed_bytes: all_files[0].len,
            }
        );
        assert!(ct.queue_pieces[0]);
        assert!(!ct.queue_pieces[1]);
        assert!(!ct.queue_pieces[2]);

        // Select only the second file.
        assert_eq!(
            ct.update_only_files(&all_files, &HashSet::from_iter([1]))
                .unwrap(),
            HaveNeededSelected {
                have_bytes: 0,
                selected_bytes: piece_len as u64,
                needed_bytes: piece_len as u64,
            }
        );
        assert!(!ct.queue_pieces[0]);
        assert!(ct.queue_pieces[1]);
        assert!(!ct.queue_pieces[2]);

        // Select only the third file (zero sized one!).
        assert_eq!(
            ct.update_only_files(&all_files, &HashSet::from_iter([2]))
                .unwrap(),
            HaveNeededSelected {
                have_bytes: 0,
                selected_bytes: 0,
                needed_bytes: 0,
            }
        );
        assert!(!ct.queue_pieces[0]);
        assert!(!ct.queue_pieces[1]);
        assert!(!ct.queue_pieces[2]);

        // Select only the fourth file.
        assert_eq!(
            ct.update_only_files(&all_files, &HashSet::from_iter([3]))
                .unwrap(),
            HaveNeededSelected {
                have_bytes: 0,
                selected_bytes: (piece_len + 1) as u64,
                needed_bytes: (piece_len + 1) as u64,
            }
        );
        assert!(!ct.queue_pieces[0]);
        assert!(ct.queue_pieces[1]);
        assert!(ct.queue_pieces[2]);

        // Select first and last file
        assert_eq!(
            ct.update_only_files(&all_files, &HashSet::from_iter([0, 3]))
                .unwrap(),
            HaveNeededSelected {
                have_bytes: 0,
                selected_bytes: all_files[0].len + all_files[3].len + 1,
                needed_bytes: all_files[0].len + all_files[3].len + 1,
            }
        );
        assert!(ct.queue_pieces[0]);
        assert!(ct.queue_pieces[1]);
        assert!(ct.queue_pieces[2]);

        // Select all files
        assert_eq!(
            ct.update_only_files(&all_files, &HashSet::from_iter([0, 1, 2, 3]))
                .unwrap(),
            HaveNeededSelected {
                have_bytes: 0,
                selected_bytes: total_len,
                needed_bytes: total_len
            }
        );
        assert!(ct.queue_pieces[0]);
        assert!(ct.queue_pieces[1]);
        assert!(ct.queue_pieces[2]);
    }
}
