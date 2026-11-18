use std::collections::{HashMap, HashSet};

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

    // Chunks (blocks) written to the output file, for pieces that are partly
    // written, or fully written and waiting for their hash check. Written doesn't
    // mean valid yet. A piece in "have" has all its chunks; any other piece that
    // isn't here has none.
    //
    // Only in-flight pieces need per-chunk state, so this is kept sparse. A dense
    // bitfield with one bit per chunk of the whole torrent costs total_size / 16 KiB
    // bits whether or not anything is downloading: about 27 MB for a session
    // seeding 3.5 TB.
    chunk_status: HashMap<u32, BF>,

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

#[derive(Debug, PartialEq, Eq)]
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

        if have_pieces.as_slice().len() < lengths.total_pieces() as usize {
            anyhow::bail!(
                "bug: have_pieces.len() < lengths.total_pieces(); {} < {}",
                have_pieces.as_slice().len(),
                lengths.total_pieces()
            );
        }

        let mut ct = Self {
            chunk_status: HashMap::new(),
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
        self.chunk_status.remove(&index.get());
    }

    pub fn mark_piece_downloaded(&mut self, idx: ValidPieceIndex) {
        let id = idx.get() as usize;
        // While its hash was being checked the piece was neither in-flight nor have, and
        // anything that queues "selected and not have" - update_only_files() re-selecting
        // its file - queued it. A piece we have is not one a peer should be handed, so
        // whatever queued it in that window is undone here.
        self.queue_pieces.set(id, false);
        // "have" now says all its chunks are written.
        self.chunk_status.remove(&idx.get());
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
        Some(self.mark_chunk_info_downloaded(&chunk_info))
    }

    fn mark_chunk_info_downloaded(&mut self, chunk_info: &ChunkInfo) -> ChunkMarkingResult {
        let index = chunk_info.piece_index;
        let have = self
            .have
            .as_slice()
            .get(index.get() as usize)
            .map(|b| *b)
            .unwrap_or(false);
        if have {
            return ChunkMarkingResult::PreviouslyCompleted;
        }
        let chunks = self.lengths.chunks_per_piece(index) as usize;
        let bits = self.chunk_status.entry(index.get()).or_insert_with(|| {
            bitvec::vec::BitVec::<u8, bitvec::order::Msb0>::repeat(false, chunks)
                .into_boxed_bitslice()
        });
        if bits.all() {
            return ChunkMarkingResult::PreviouslyCompleted;
        }
        bits.set(chunk_info.chunk_index as usize, true);
        trace!(
            "piece={}, chunk_info={:?}, bits={:?}",
            index, chunk_info, bits,
        );

        if bits.all() {
            return ChunkMarkingResult::Completed;
        }
        ChunkMarkingResult::NotCompleted
    }

    #[cfg(test)]
    fn pieces_with_chunk_state(&self) -> usize {
        self.chunk_status.len()
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

    use librqbit_core::lengths::ChunkInfo;

    use super::{ChunkMarkingResult, ChunkTracker};

    fn chunk(l: &Lengths, piece: u32, chunk: u32) -> ChunkInfo {
        let piece = l.validate_piece_index(piece).unwrap();
        let begin = chunk * CHUNK_SIZE;
        let size = l.chunk_size(piece, chunk).unwrap();
        l.chunk_info_from_received_data(piece, begin, size).unwrap()
    }

    #[test]
    fn test_chunk_marking() {
        // Create the most obnoxious lengths, and ensure it doesn't break in that case:
        // three pieces of three chunks, the last piece one byte long.
        let piece_length = CHUNK_SIZE * 2 + 1;
        let l = Lengths::new(piece_length as u64 * 2 + 1, piece_length).unwrap();
        assert_eq!(l.total_pieces(), 3);
        assert_eq!(l.default_chunks_per_piece(), 3);
        assert_eq!(l.total_chunks(), 7);

        let bf_len = l.piece_bitfield_bytes();
        let mut have = BF::from_boxed_slice(vec![0u8; bf_len].into_boxed_slice());
        have.set(1, true);
        let selected = BF::from_boxed_slice(vec![u8::MAX; bf_len].into_boxed_slice());
        let mut ct = ChunkTracker::new(have.into_dyn(), selected, l, &Default::default()).unwrap();

        // Nothing is in flight yet, so nothing is kept per chunk.
        assert_eq!(ct.pieces_with_chunk_state(), 0);

        // A piece we have: every chunk counts as written already.
        assert_eq!(
            ct.mark_chunk_info_downloaded(&chunk(&l, 1, 0)),
            ChunkMarkingResult::PreviouslyCompleted
        );
        assert_eq!(ct.pieces_with_chunk_state(), 0);

        // Piece 0 fills chunk by chunk.
        assert_eq!(
            ct.mark_chunk_info_downloaded(&chunk(&l, 0, 2)),
            ChunkMarkingResult::NotCompleted
        );
        assert_eq!(
            ct.mark_chunk_info_downloaded(&chunk(&l, 0, 0)),
            ChunkMarkingResult::NotCompleted
        );
        assert_eq!(
            ct.mark_chunk_info_downloaded(&chunk(&l, 0, 1)),
            ChunkMarkingResult::Completed
        );
        assert_eq!(
            ct.mark_chunk_info_downloaded(&chunk(&l, 0, 1)),
            ChunkMarkingResult::PreviouslyCompleted
        );
        assert_eq!(ct.pieces_with_chunk_state(), 1);

        // Its hash check fails: it starts over.
        let p0 = l.validate_piece_index(0).unwrap();
        ct.mark_piece_broken_if_not_have(p0);
        assert_eq!(ct.pieces_with_chunk_state(), 0);
        assert_eq!(
            ct.mark_chunk_info_downloaded(&chunk(&l, 0, 0)),
            ChunkMarkingResult::NotCompleted
        );

        // The last piece has a single one-byte chunk.
        assert_eq!(
            ct.mark_chunk_info_downloaded(&chunk(&l, 2, 0)),
            ChunkMarkingResult::Completed
        );
        // Its check passes: "have" takes over and the chunk state goes.
        ct.mark_piece_downloaded(l.validate_piece_index(2).unwrap());
        assert_eq!(ct.pieces_with_chunk_state(), 1);
        assert_eq!(
            ct.mark_chunk_info_downloaded(&chunk(&l, 2, 0)),
            ChunkMarkingResult::PreviouslyCompleted
        );

        // A broken piece we already have stays as it is.
        ct.mark_piece_broken_if_not_have(l.validate_piece_index(1).unwrap());
        assert_eq!(
            ct.mark_chunk_info_downloaded(&chunk(&l, 1, 2)),
            ChunkMarkingResult::PreviouslyCompleted
        );
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

    // Between its last chunk arriving and its hash passing, a piece is neither queued,
    // in-flight nor have. update_only_files() sees "selected and not have" and queues it,
    // and the hash then passes: without this, the piece is have AND queued, and the next
    // peer to ask is handed a piece we have.
    #[test]
    fn test_a_piece_queued_during_its_hash_check_is_dequeued_when_it_passes() {
        let piece_len = CHUNK_SIZE * 2;
        let l = Lengths::new(piece_len as u64 * 3, piece_len).unwrap();
        let files = vec![FileInfo {
            relative_filename: "0".into(),
            offset_in_torrent: 0,
            piece_range: 0..3,
            len: piece_len as u64 * 3,
            attrs: Default::default(),
        }];
        let bf_len = l.piece_bitfield_bytes();
        let have = BF::from_boxed_slice(vec![0u8; bf_len].into_boxed_slice());
        let mut selected = BF::from_boxed_slice(vec![0u8; bf_len].into_boxed_slice());
        selected.get_mut(0..3).unwrap().fill(true);
        let mut ct = ChunkTracker::new(have.into_dyn(), selected, l, &files).unwrap();
        let p0 = l.validate_piece_index(0).unwrap();

        // A peer takes the piece, delivers all of it, and the hash check starts: it is
        // out of the queue and, in the real thing, just out of the in-flight map too.
        ct.reserve_needed_piece(p0);
        assert!(!ct.queue_pieces[0]);

        // Meanwhile the user deselects the file and selects it again.
        ct.update_only_files(&files, &HashSet::new()).unwrap();
        ct.update_only_files(&files, &HashSet::from_iter([0]))
            .unwrap();
        assert!(ct.queue_pieces[0]);

        // The hash passes.
        ct.mark_piece_downloaded(p0);
        assert!(ct.is_piece_have(p0));
        assert!(
            !ct.queue_pieces[0],
            "a piece we have is still queued, so a second peer will download it again"
        );
    }
}
