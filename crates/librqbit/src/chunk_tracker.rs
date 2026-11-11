use anyhow::Context;
use buffers::ByteBuf;
use librqbit_core::lengths::{ChunkInfo, Lengths, ValidPieceIndex};
use peer_binary_protocol::Piece;
use tracing::{debug, trace};

use crate::{
    bitv::{BitV, BoxBitV},
    file_info::FileInfo,
    priority::PieceLevels,
    type_aliases::{BF, BS, FileInfos},
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

    // The pieces with a non-zero level in `levels`: the ones to download. Changes only with
    // set_piece_levels().
    selected: BF,

    // The effective priority level of every piece.
    levels: PieceLevels,

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
    // How many bytes the user selected (the pieces with a non-zero priority).
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
        // The effective priority of every piece; the ones above 0 are downloaded.
        levels: PieceLevels,
        lengths: Lengths,
        file_infos: &FileInfos,
    ) -> anyhow::Result<Self> {
        if levels.len() != lengths.total_pieces() as usize {
            anyhow::bail!(
                "bug: {} piece levels for {} pieces",
                levels.len(),
                lengths.total_pieces()
            );
        }
        let selected_pieces = levels.selected(&lengths);
        let needed_pieces = compute_queued_pieces(have_pieces.as_slice(), &selected_pieces)
            .context("error computing needed pieces")?;

        // TODO: ideally this needs to be a list based on needed files, e.g.
        // last needed piece for each file. But let's keep simple for now.

        let mut ct = Self {
            chunk_status: compute_chunk_have_status(&lengths, have_pieces.as_slice())
                .context("error computing chunk status")?,
            queue_pieces: needed_pieces,
            selected: selected_pieces,
            levels,
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

    /// Pieces to download next: selected, not had, not in flight.
    pub(crate) fn queued_pieces(&self) -> &BS {
        &self.queue_pieces
    }

    pub(crate) fn is_piece_have(&self, id: ValidPieceIndex) -> bool {
        self.have.as_slice()[id.get() as usize]
    }

    /// Puts a piece we don't have back in the queue and forgets its chunks, for it to be
    /// downloaded again from scratch. A piece that is not selected (priority 0) is not queued:
    /// a stream may still fetch it, as streams ask for pieces outside the queue.
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
        let selected = self.selected.get(index.get() as usize).is_some_and(|b| *b);
        self.queue_pieces.set(index.get() as usize, selected);
        if let Some(s) = self.chunk_status.get_mut(self.lengths.chunk_range(index)) {
            s.fill(false);
        }
    }

    pub fn mark_piece_downloaded(&mut self, idx: ValidPieceIndex) {
        let id = idx.get() as usize;
        // While its hash was being checked the piece was neither in-flight nor have, and
        // anything that queues "selected and not have" - set_piece_levels() re-selecting
        // its file - queued it. A piece we have is not one a peer should be handed, so
        // whatever queued it in that window is undone here.
        self.queue_pieces.set(id, false);
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

    /// Changes the effective priority of the pieces. Pieces that become wanted are queued
    /// unless we have them or `is_inflight` says a peer is already downloading them; pieces that
    /// become unwanted leave the queue (the caller releases the in-flight ones). Data on disk and
    /// the have-bitfield are not touched. Returns the new byte accounting.
    pub fn set_piece_levels(
        &mut self,
        levels: PieceLevels,
        is_inflight: impl Fn(ValidPieceIndex) -> bool,
    ) -> anyhow::Result<HaveNeededSelected> {
        if levels.len() != self.lengths.total_pieces() as usize {
            anyhow::bail!(
                "bug: {} piece levels for {} pieces",
                levels.len(),
                self.lengths.total_pieces()
            );
        }
        let selected = levels.selected(&self.lengths);
        let prev_selected = std::mem::replace(&mut self.selected, selected);
        self.levels = levels;

        // prev_selected=false and selected=true and have=false: requeue the piece
        for idx in 0..self.lengths.total_pieces() {
            let i = idx as usize;
            if !self.selected[i] || prev_selected[i] || self.have.as_slice()[i] {
                continue;
            }
            if let Some(idx) = self.lengths.validate_piece_index(idx)
                && !is_inflight(idx)
            {
                self.mark_piece_broken_if_not_have(idx);
            }
        }

        // selected=false: don't need the piece - stop queueing it. A piece we have is never
        // queued, so this is "queue = queue & selected".
        let selected = self.selected.clone();
        self.queue_pieces &= selected;

        self.hns = self.calc_hns();
        Ok(self.hns)
    }

    pub(crate) fn get_selected_pieces(&self) -> &BF {
        &self.selected
    }

    /// The effective priority of every piece.
    pub(crate) fn piece_levels(&self) -> &PieceLevels {
        &self.levels
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
    use librqbit_core::{
        constants::CHUNK_SIZE,
        lengths::{Lengths, ValidPieceIndex},
        torrent_metainfo::FileDetailsAttrs,
    };
    use std::collections::HashSet;

    use crate::{
        bitv::BitV,
        chunk_tracker::HaveNeededSelected,
        file_info::FileInfo,
        priority::{PieceLevels, Priorities},
        type_aliases::{BF, FileInfos},
    };

    use super::{ChunkTracker, compute_chunk_have_status};

    fn all_selected(l: &Lengths) -> PieceLevels {
        PieceLevels::new(vec![1u8; l.total_pieces() as usize].into_boxed_slice())
    }

    fn levels_of(p: &Priorities, l: &Lengths, files: &FileInfos) -> PieceLevels {
        p.piece_levels(l, files)
    }

    // The selection of update_only_files(), the way the torrent applies it.
    fn select_only(
        ct: &mut ChunkTracker,
        files: &FileInfos,
        only: impl IntoIterator<Item = usize>,
    ) -> HaveNeededSelected {
        let mut p = Priorities::new(files);
        p.set_only_files(files, &HashSet::from_iter(only)).unwrap();
        let levels = levels_of(&p, &ct.lengths.clone(), files);
        ct.set_piece_levels(levels, |_| false).unwrap()
    }

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
            all_selected(&l),
            l,
            &all_files,
        )
        .unwrap();

        // Select all file, no changes.
        assert_eq!(
            select_only(&mut ct, &all_files, [0, 1, 2, 3]),
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
            select_only(&mut ct, &all_files, [0]),
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
            select_only(&mut ct, &all_files, [1]),
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
            select_only(&mut ct, &all_files, [2]),
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
            select_only(&mut ct, &all_files, [3]),
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
            select_only(&mut ct, &all_files, [0, 3]),
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
            select_only(&mut ct, &all_files, [0, 1, 2, 3]),
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
    // in-flight nor have. set_piece_levels() sees "selected and not have" and queues it,
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
        let mut ct = ChunkTracker::new(
            BF::from_boxed_slice(vec![0u8; l.piece_bitfield_bytes()].into_boxed_slice()).into_dyn(),
            all_selected(&l),
            l,
            &files,
        )
        .unwrap();
        let p0 = l.validate_piece_index(0).unwrap();

        // A peer takes the piece, delivers all of it, and the hash check starts: it is
        // out of the queue and, in the real thing, just out of the in-flight map too.
        ct.reserve_needed_piece(p0);
        assert!(!ct.queue_pieces[0]);

        // Meanwhile the user deselects the file and selects it again.
        select_only(&mut ct, &files, []);
        select_only(&mut ct, &files, [0]);
        assert!(ct.queue_pieces[0]);

        // The hash passes.
        ct.mark_piece_downloaded(p0);
        assert!(ct.is_piece_have(p0));
        assert!(
            !ct.queue_pieces[0],
            "a piece we have is still queued, so a second peer will download it again"
        );
    }

    // Three files over 4 pieces of 2 chunks: "a" is pieces 0..=1 and the first half of 2,
    // "b" the second half of 2 and piece 3 minus its last 100 bytes, which are "c".
    fn shared_boundary_files() -> (FileInfos, Lengths, u32) {
        let piece_len = CHUNK_SIZE * 2;
        let p = u64::from(piece_len);
        let l = Lengths::new(p * 4, piece_len).unwrap();
        let file = |name: &str, offset: u64, len: u64| {
            let first = u32::try_from(offset / p).unwrap();
            let end = u32::try_from((offset + len).div_ceil(p)).unwrap();
            FileInfo {
                relative_filename: name.into(),
                offset_in_torrent: offset,
                piece_range: first..end,
                len,
                attrs: Default::default(),
            }
        };
        let files = vec![
            file("a", 0, p * 2 + p / 2),
            file("b", p * 2 + p / 2, p + p / 2 - 100),
            file("c", p * 4 - 100, 100),
        ];
        (files, l, piece_len)
    }

    fn idx(l: &Lengths, i: u32) -> ValidPieceIndex {
        l.validate_piece_index(i).unwrap()
    }

    fn have_none(l: &Lengths) -> BF {
        BF::from_boxed_slice(vec![0u8; l.piece_bitfield_bytes()].into_boxed_slice())
    }

    #[test]
    fn accounting_follows_every_kind_of_level_change() {
        let (files, l, piece_len) = shared_boundary_files();
        let p = u64::from(piece_len);
        let mut prio = Priorities::new(&files);
        let mut ct = ChunkTracker::new(
            have_none(&l).into_dyn(),
            levels_of(&prio, &l, &files),
            l,
            &files,
        )
        .unwrap();
        let hns = |have, needed, selected| HaveNeededSelected {
            have_bytes: have,
            needed_bytes: needed,
            selected_bytes: selected,
        };
        assert_eq!(*ct.get_hns(), hns(0, 4 * p, 4 * p));

        // Piece 2 (shared by "a" and "b") arrives.
        ct.reserve_needed_piece(idx(&l, 2));
        ct.mark_piece_downloaded(idx(&l, 2));
        assert_eq!(*ct.get_hns(), hns(p, 3 * p, 4 * p));

        let mut apply = |prio: &mut Priorities, changes: &[(usize, u8)]| {
            prio.set_files(&files, changes).unwrap();
            let levels = levels_of(prio, &l, &files);
            ct.set_piece_levels(levels, |_| false).unwrap();
            (*ct.get_hns(), ct.queued_pieces().to_bitvec())
        };

        // n -> m: nothing changes but the levels.
        let (h, q) = apply(&mut prio, &[(0, 7), (1, 6), (2, 2)]);
        assert_eq!(h, hns(p, 3 * p, 4 * p));
        assert_eq!(q.iter_ones().collect::<Vec<_>>(), vec![0, 1, 3]);

        // n -> 0 on "a": pieces 0 and 1 go, 2 stays (had, and "b" wants it).
        let (h, q) = apply(&mut prio, &[(0, 0)]);
        assert_eq!(h, hns(p, p, 2 * p));
        assert_eq!(q.iter_ones().collect::<Vec<_>>(), vec![3]);

        // "b" and "c" to 0 as well: nothing is wanted, and that is finished.
        let (h, q) = apply(&mut prio, &[(1, 0), (2, 0)]);
        assert_eq!(h, hns(p, 0, 0));
        assert!(h.finished());
        assert_eq!(q.count_ones(), 0);

        // 0 -> n on "c" alone: the piece it shares with "b" is wanted again; unfinished.
        let (h, q) = apply(&mut prio, &[(2, 1)]);
        assert_eq!(h, hns(p, p, p));
        assert!(!h.finished());
        assert_eq!(q.iter_ones().collect::<Vec<_>>(), vec![3]);

        // It arrives: finished.
        ct.reserve_needed_piece(idx(&l, 3));
        ct.mark_piece_downloaded(idx(&l, 3));
        assert!(ct.get_hns().finished());
        assert_eq!(*ct.get_hns(), hns(2 * p, 0, p));

        // 0 -> n on "a": unfinished again, needing 0 and 1 only.
        prio.set_files(&files, &[(0, 1)]).unwrap();
        let h = ct
            .set_piece_levels(levels_of(&prio, &l, &files), |_| false)
            .unwrap();
        assert_eq!(h, hns(2 * p, 2 * p, 4 * p));
        assert_eq!(
            ct.queued_pieces().iter_ones().collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(ct.get_hns(), &ct.calc_hns());
    }

    #[test]
    fn piece_overrides_count_like_files() {
        let (files, l, piece_len) = shared_boundary_files();
        let p = u64::from(piece_len);
        let mut prio = Priorities::new(&files);
        prio.set_files(&files, &[(0, 0), (1, 0), (2, 0)]).unwrap();
        prio.set_pieces(4, &[(1, Some(3))]).unwrap();
        let mut ct = ChunkTracker::new(
            have_none(&l).into_dyn(),
            levels_of(&prio, &l, &files),
            l,
            &files,
        )
        .unwrap();
        assert_eq!(ct.get_hns().selected_bytes, p);
        assert_eq!(ct.queued_pieces().iter_ones().collect::<Vec<_>>(), vec![1]);
        prio.set_pieces(4, &[(1, Some(0)), (3, Some(5))]).unwrap();
        ct.set_piece_levels(levels_of(&prio, &l, &files), |_| false)
            .unwrap();
        assert_eq!(ct.piece_levels().as_slice(), &[0, 0, 0, 5]);
        assert_eq!(ct.queued_pieces().iter_ones().collect::<Vec<_>>(), vec![3]);
        assert_eq!(ct.get_hns().needed_bytes, p);
    }

    #[test]
    fn padding_files_are_not_selected_bytes() {
        let piece_len = CHUNK_SIZE * 2;
        let p = u64::from(piece_len);
        let l = Lengths::new(p * 2, piece_len).unwrap();
        let files = vec![
            FileInfo {
                relative_filename: "a".into(),
                offset_in_torrent: 0,
                piece_range: 0..1,
                len: 100,
                attrs: Default::default(),
            },
            FileInfo {
                relative_filename: ".pad/1".into(),
                offset_in_torrent: 100,
                piece_range: 0..1,
                len: p - 100,
                attrs: FileDetailsAttrs {
                    padding: true,
                    ..Default::default()
                },
            },
            FileInfo {
                relative_filename: "b".into(),
                offset_in_torrent: p,
                piece_range: 1..2,
                len: p,
                attrs: Default::default(),
            },
        ];
        let mut prio = Priorities::new(&files);
        prio.set_files(&files, &[(0, 0), (1, 7)]).unwrap();
        let ct = ChunkTracker::new(
            have_none(&l).into_dyn(),
            levels_of(&prio, &l, &files),
            l,
            &files,
        )
        .unwrap();
        // The padding file asked for 7 but stays 0, so piece 0 is not wanted.
        assert_eq!(ct.piece_levels().as_slice(), &[0, 1]);
        assert_eq!(ct.get_hns().selected_bytes, p);
    }

    #[test]
    fn in_flight_and_unselected_pieces_are_not_queued() {
        let (files, l, _) = shared_boundary_files();
        let mut prio = Priorities::new(&files);
        prio.set_files(&files, &[(0, 0)]).unwrap();
        let mut ct = ChunkTracker::new(
            have_none(&l).into_dyn(),
            levels_of(&prio, &l, &files),
            l,
            &files,
        )
        .unwrap();
        // A stream fetched piece 0 (unselected) and its hash failed: it is not queued.
        ct.mark_piece_broken_if_not_have(idx(&l, 0));
        assert!(!ct.queued_pieces()[0]);

        // Piece 1 is being downloaded (by a stream) when "a" is selected: a peer already has
        // it, so it is not queued for a second one.
        prio.set_files(&files, &[(0, 1)]).unwrap();
        ct.set_piece_levels(levels_of(&prio, &l, &files), |p| p.get() == 1)
            .unwrap();
        assert_eq!(
            ct.queued_pieces().iter_ones().collect::<Vec<_>>(),
            vec![0, 2, 3]
        );
    }
}
