//! Which queued piece a peer is asked for next.
//!
//! Follows libtorrent's piece picker (the one qBittorrent uses):
//!
//! - **Rarest first** by default: among the queued pieces the peer has, take the one the fewest
//!   connected peers have, ties broken at random. Rare pieces spread through the swarm before
//!   their holders leave, and peers downloading together ask each other for different pieces.
//! - The first few pieces are picked at random instead (libtorrent's `initial_picker_threshold`),
//!   so a fresh peer quickly has whole pieces to trade.
//! - **Sequential** (opt-in per torrent): pieces in file order, for playing while downloading.
//! - **First and last pieces first** (opt-in per torrent): the first and last 1% of every selected
//!   file go ahead of everything else, whatever the mode. Media players read a file's header and
//!   index from there (qBittorrent's `applyFirstLastPiecePriority`).
//!
//! Streams (`TorrentStreams`) and steals from slow peers are decided by the caller before and
//! after this; this module only orders the queue.

use std::{
    cmp::Reverse,
    ops::Range,
    sync::atomic::{AtomicU32, Ordering},
};

use librqbit_core::lengths::Lengths;
use rand::RngExt;
use serde::{Deserialize, Serialize};

use crate::{
    priority::PieceLevels,
    type_aliases::{BF, BS, FileInfos},
};

/// How a torrent orders the pieces it downloads.
///
/// The default (both off) is rarest first.
///
/// ```
/// let picking = librqbit::PiecePicking {
///     sequential: true,
///     ..Default::default()
/// };
/// assert!(!picking.first_last_pieces_first);
/// ```
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiecePicking {
    /// Download pieces in file order (files sorted by name) instead of rarest first.
    ///
    /// Lets a file be played while it downloads, at the cost of the swarm: everyone asks for the
    /// same pieces, and rare ones are not saved before their holders leave.
    #[serde(default)]
    pub sequential: bool,
    /// Download the first and last 1% of every selected file (at least one piece each) before
    /// anything else, so media players can open the file early.
    #[serde(default)]
    pub first_last_pieces_first: bool,
}

/// How many pieces are picked at random before rarest first takes over.
const INITIAL_RANDOM_PIECES: u64 = 4;

/// How many connected peers have each piece.
///
/// Peers that had the whole torrent when their bitfield arrived are counted once in `seeds`
/// instead of once per piece. Updated from the peer table without the torrent lock, so a read
/// may be a few messages behind; the picker only needs it to be roughly right.
pub(crate) struct PieceAvailability {
    counts: Box<[AtomicU32]>,
    seeds: AtomicU32,
}

fn decrement(counter: &AtomicU32) {
    let mut current = counter.load(Ordering::Relaxed);
    while current > 0 {
        match counter.compare_exchange_weak(
            current,
            current - 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(actual) => current = actual,
        }
    }
    debug_assert!(false, "bug: piece availability went below zero");
}

impl PieceAvailability {
    pub fn new(total_pieces: u32) -> Self {
        Self {
            counts: (0..total_pieces).map(|_| AtomicU32::new(0)).collect(),
            seeds: AtomicU32::new(0),
        }
    }

    pub fn add_seed(&self) {
        self.seeds.fetch_add(1, Ordering::Relaxed);
    }

    pub fn remove_seed(&self) {
        decrement(&self.seeds);
    }

    pub fn add_piece(&self, piece: usize) {
        if let Some(c) = self.counts.get(piece) {
            c.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Count every piece set in `bits`. Bits past the last piece are ignored.
    pub fn add_pieces(&self, bits: &BS) {
        for piece in bits.iter_ones() {
            self.add_piece(piece);
        }
    }

    /// Undo [`Self::add_pieces`] for the same bits.
    pub fn remove_pieces(&self, bits: &BS) {
        for piece in bits.iter_ones() {
            if let Some(c) = self.counts.get(piece) {
                decrement(c);
            }
        }
    }

    pub fn get(&self, piece: usize) -> u32 {
        let count = self
            .counts
            .get(piece)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or_default();
        count.saturating_add(self.seeds.load(Ordering::Relaxed))
    }
}

struct OrderedFile {
    pieces: Range<usize>,
    len: u64,
    // Level above 0. Only these get first and last pieces first; the others can still have
    // pieces queued through a piece override, so sequential mode walks them too.
    wanted: bool,
}

/// Picks the next piece for a peer, per the torrent's [`PiecePicking`] and the priority levels
/// of its files and pieces.
pub(crate) struct PiecePicker {
    options: PiecePicking,
    lengths: Lengths,
    // Non-padding files in the order sequential mode downloads them: the wanted ones (level
    // above 0) by name, then the others by name. The others only hold queued pieces through a
    // piece override, and a piece they share with a wanted file keeps its place in that file.
    files: Vec<OrderedFile>,
    // The pieces "first and last pieces first" puts ahead of the rest. None when it's off.
    edge_pieces: Option<BF>,
    // Breaks availability ties. Differs per torrent start, so clients don't all agree on an order.
    salt: u64,
}

// splitmix64 finalizer: a cheap, well-spread hash for random-looking tie breaks.
fn mix(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

impl PiecePicker {
    /// `file_levels` is the priority of every file; files at 0 are not downloaded.
    pub fn new(
        options: PiecePicking,
        lengths: Lengths,
        file_infos: &FileInfos,
        file_levels: &[u8],
    ) -> Self {
        let mut picker = Self {
            options,
            lengths,
            files: Vec::new(),
            edge_pieces: None,
            salt: rand::rng().random(),
        };
        picker.update_files(file_infos, file_levels);
        picker
    }

    pub fn set_options(&mut self, options: PiecePicking) {
        self.options = options;
        self.edge_pieces = self.compute_edge_pieces();
    }

    /// Follows a change of the files' priorities.
    pub fn update_files(&mut self, file_infos: &FileInfos, file_levels: &[u8]) {
        let mut files: Vec<(&crate::file_info::FileInfo, bool)> = file_infos
            .iter()
            .enumerate()
            .filter(|(_, fi)| !fi.attrs.padding)
            .map(|(id, fi)| (fi, file_levels.get(id).is_some_and(|l| *l > 0)))
            .collect();
        // Wanted files first, each group by name, as many torrents list their files in no
        // particular order.
        files.sort_by(|a, b| (!a.1, &a.0.relative_filename).cmp(&(!b.1, &b.0.relative_filename)));
        self.files = files
            .into_iter()
            .map(|(fi, wanted)| OrderedFile {
                pieces: fi.piece_range_usize(),
                len: fi.len,
                wanted,
            })
            .collect();
        self.edge_pieces = self.compute_edge_pieces();
    }

    fn compute_edge_pieces(&self) -> Option<BF> {
        if !self.options.first_last_pieces_first {
            return None;
        }
        let mut edge =
            BF::from_boxed_slice(vec![0u8; self.lengths.piece_bitfield_bytes()].into_boxed_slice());
        // qBittorrent: ceil(file size * 1% / piece length) at each end. The 1% covers an AVI
        // index at the end of the file in the worst case.
        let per_end = u64::from(self.lengths.default_piece_length()).saturating_mul(100);
        for f in self.files.iter().filter(|f| f.wanted) {
            let n = usize::try_from(f.len.div_ceil(per_end))
                .unwrap_or(usize::MAX)
                .min(f.pieces.len());
            let head = f.pieces.start..f.pieces.start + n;
            let tail = f.pieces.end - n..f.pieces.end;
            for r in [head, tail] {
                if let Some(bits) = edge.get_mut(r) {
                    bits.fill(true);
                }
            }
        }
        Some(edge)
    }

    /// The next piece to ask this peer for, out of `queue` (queued: selected, not had, not in
    /// flight). `levels` is the priority of every piece: a higher level always goes first, and
    /// the options above order the pieces inside one level. `queued_top` is the highest level
    /// in `queue`. `have_bytes` is how much of the torrent is verified.
    pub fn pick(
        &self,
        queue: &BS,
        levels: &PieceLevels,
        queued_top: u8,
        have_bytes: u64,
        availability: &PieceAvailability,
        peer_has_piece: impl Fn(usize) -> bool,
    ) -> Option<usize> {
        // No queued piece is above `queued_top`: a candidate there can't be beaten, so the walks
        // stop at the first one. With a single level queued (the common case) that makes every
        // walk below exactly as long as it is without priorities.
        let top = queued_top;
        // The best candidate so far. Each step only takes a piece whose level is above it (level
        // 0 is never taken), so a piece found earlier wins inside its level.
        let mut best: Option<(u8, usize)> = None;
        let floor = |best: Option<(u8, usize)>| best.map_or(0, |(level, _)| level);

        // libtorrent's top priority disregards availability, and these are few: lowest index wins.
        if let Some(edge) = &self.edge_pieces {
            for p in edge.iter_ones() {
                let level = levels.get(p);
                if level > floor(best) && queue.get(p).is_some_and(|b| *b) && peer_has_piece(p) {
                    if level >= top {
                        return Some(p);
                    }
                    best = Some((level, p));
                }
            }
        }

        if self.options.sequential {
            for f in &self.files {
                let Some(bits) = queue.get(f.pieces.clone()) else {
                    continue;
                };
                for p in bits.iter_ones().map(|i| f.pieces.start + i) {
                    let level = levels.get(p);
                    if level > floor(best) && peer_has_piece(p) {
                        if level >= top {
                            return Some(p);
                        }
                        best = Some((level, p));
                    }
                }
            }
            return best.map(|(_, p)| p);
        }

        let random =
            have_bytes < INITIAL_RANDOM_PIECES * u64::from(self.lengths.default_piece_length());
        let floor = floor(best);
        queue
            .iter_ones()
            .filter(|p| levels.get(*p) > floor && peer_has_piece(*p))
            .min_by_key(|p| {
                let rarity = if random { 0 } else { availability.get(*p) };
                (Reverse(levels.get(*p)), rarity, mix(self.salt ^ *p as u64))
            })
            .or(best.map(|(_, p)| p))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use librqbit_core::lengths::Lengths;

    use super::{PieceAvailability, PiecePicker, PiecePicking};
    use crate::{
        file_info::FileInfo,
        priority::{PieceLevels, Priorities},
        type_aliases::{BF, FileInfos},
    };

    const PIECE: u32 = 16384;

    fn file(name: &str, first_piece: u32, pieces: u32) -> FileInfo {
        FileInfo {
            relative_filename: name.into(),
            offset_in_torrent: u64::from(first_piece) * u64::from(PIECE),
            piece_range: first_piece..first_piece + pieces,
            attrs: Default::default(),
            len: u64::from(pieces) * u64::from(PIECE),
        }
    }

    fn lengths(pieces: u32) -> Lengths {
        Lengths::new(u64::from(pieces) * u64::from(PIECE), PIECE).unwrap()
    }

    fn all_queued(pieces: u32) -> BF {
        let mut bf = BF::from_boxed_slice(
            vec![0u8; lengths(pieces).piece_bitfield_bytes()].into_boxed_slice(),
        );
        bf[..pieces as usize].fill(true);
        bf
    }

    // Past the initial random phase.
    fn settled(pieces: u32) -> u64 {
        u64::from(pieces) * u64::from(PIECE)
    }

    // The highest level in `queue`, as ChunkTracker counts it.
    fn top(queue: &BF, lv: &PieceLevels) -> u8 {
        queue.iter_ones().map(|p| lv.get(p)).max().unwrap_or(0)
    }

    // Every piece normal.
    fn flat(pieces: u32) -> PieceLevels {
        PieceLevels::new(vec![1u8; pieces as usize].into_boxed_slice())
    }

    fn normal_files(files: &FileInfos) -> Vec<u8> {
        vec![1; files.len()]
    }

    // The effective piece levels of files at `file_levels` with `overrides`.
    fn levels(
        files: &FileInfos,
        pieces: u32,
        file_levels: &[u8],
        overrides: &[(u32, u8)],
    ) -> PieceLevels {
        Priorities::from_options(files, pieces, None, Some(file_levels), Some(overrides))
            .unwrap()
            .piece_levels(&lengths(pieces), files)
    }

    #[test]
    fn rarest_piece_the_peer_has_wins() {
        let files: FileInfos = vec![file("a", 0, 8)];
        let picker = PiecePicker::new(
            PiecePicking::default(),
            lengths(8),
            &files,
            &normal_files(&files),
        );
        let lv = flat(8);
        let avail = PieceAvailability::new(8);
        for p in 0..8 {
            for _ in 0..(10 - p) {
                avail.add_piece(p);
            }
        }
        let queue = all_queued(8);
        // Piece 7 is the rarest overall.
        assert_eq!(
            picker.pick(&queue, &lv, top(&queue, &lv), settled(8), &avail, |_| true),
            Some(7)
        );
        // The peer doesn't have 7 or 6: 5 is the rarest it has.
        assert_eq!(
            picker.pick(&queue, &lv, top(&queue, &lv), settled(8), &avail, |p| p < 6),
            Some(5)
        );
        // Not queued (in flight / had) pieces are skipped.
        let mut queue = queue;
        queue.set(7, false);
        assert_eq!(
            picker.pick(&queue, &lv, top(&queue, &lv), settled(8), &avail, |_| true),
            Some(6)
        );
    }

    #[test]
    fn seeds_count_toward_every_piece() {
        let avail = PieceAvailability::new(4);
        avail.add_seed();
        avail.add_seed();
        avail.add_piece(1);
        assert_eq!(avail.get(0), 2);
        assert_eq!(avail.get(1), 3);
        avail.remove_seed();
        assert_eq!(avail.get(1), 2);
    }

    #[test]
    fn ties_are_broken_differently_per_picker() {
        let files: FileInfos = vec![file("a", 0, 64)];
        let avail = PieceAvailability::new(64);
        let queue = all_queued(64);
        let lv = flat(64);
        let firsts: HashSet<usize> = (0..32)
            .filter_map(|_| {
                PiecePicker::new(
                    PiecePicking::default(),
                    lengths(64),
                    &files,
                    &normal_files(&files),
                )
                .pick(&queue, &lv, top(&queue, &lv), settled(64), &avail, |_| true)
            })
            .collect();
        assert!(firsts.len() > 1, "every picker started on the same piece");
    }

    #[test]
    fn first_pieces_ignore_availability() {
        let files: FileInfos = vec![file("a", 0, 64)];
        let avail = PieceAvailability::new(64);
        // Piece 10 is the only rare one; once settled, rarest first takes it.
        for p in 0..64 {
            if p != 10 {
                avail.add_piece(p);
            }
        }
        let queue = all_queued(64);
        let lv = flat(64);
        let picked: HashSet<usize> = (0..32)
            .filter_map(|_| {
                PiecePicker::new(
                    PiecePicking::default(),
                    lengths(64),
                    &files,
                    &normal_files(&files),
                )
                .pick(&queue, &lv, top(&queue, &lv), 0, &avail, |_| true)
            })
            .collect();
        assert!(picked.len() > 1, "initial picks followed availability");
        let picker = PiecePicker::new(
            PiecePicking::default(),
            lengths(64),
            &files,
            &normal_files(&files),
        );
        assert_eq!(
            picker.pick(&queue, &lv, top(&queue, &lv), settled(64), &avail, |_| true),
            Some(10)
        );
    }

    #[test]
    fn sequential_walks_files_by_name() {
        // Torrent order b, a; by name a (pieces 4..8) comes first.
        let files: FileInfos = vec![file("b", 0, 4), file("a", 4, 4)];
        let picking = PiecePicking {
            sequential: true,
            ..Default::default()
        };
        let picker = PiecePicker::new(picking, lengths(8), &files, &normal_files(&files));
        let lv = flat(8);
        let avail = PieceAvailability::new(8);
        let mut queue = all_queued(8);
        assert_eq!(
            picker.pick(&queue, &lv, top(&queue, &lv), settled(8), &avail, |_| true),
            Some(4)
        );
        queue.set(4, false);
        assert_eq!(
            picker.pick(&queue, &lv, top(&queue, &lv), settled(8), &avail, |_| true),
            Some(5)
        );
        assert_eq!(
            picker.pick(&queue, &lv, top(&queue, &lv), settled(8), &avail, |p| p < 4),
            Some(0)
        );
        queue[4..8].fill(false);
        assert_eq!(
            picker.pick(&queue, &lv, top(&queue, &lv), settled(8), &avail, |_| true),
            Some(0)
        );
    }

    #[test]
    fn deselected_files_are_not_walked() {
        let files: FileInfos = vec![file("a", 0, 4), file("b", 4, 4)];
        let picking = PiecePicking {
            sequential: true,
            first_last_pieces_first: true,
        };
        let picker = PiecePicker::new(picking, lengths(8), &files, &[0, 1]);
        let lv = levels(&files, 8, &[0, 1], &[]);
        let avail = PieceAvailability::new(8);
        // Even if the queue still holds a piece of "a", a level 0 piece is never picked.
        let queue = all_queued(8);
        assert_eq!(
            picker.pick(&queue, &lv, top(&queue, &lv), settled(8), &avail, |_| true),
            Some(4)
        );
        assert_eq!(
            picker.pick(&queue, &lv, top(&queue, &lv), settled(8), &avail, |p| p < 4),
            None
        );
    }

    #[test]
    fn first_and_last_pieces_go_first() {
        // 250 pieces: 1% of the file is 2.5 pieces, so 3 at each end.
        let files: FileInfos = vec![file("a", 0, 250)];
        let picking = PiecePicking {
            first_last_pieces_first: true,
            ..Default::default()
        };
        let picker = PiecePicker::new(picking, lengths(250), &files, &normal_files(&files));
        let lv = flat(250);
        let avail = PieceAvailability::new(250);
        // The middle is rarer, yet the ends go first.
        for p in [0, 1, 2, 247, 248, 249] {
            avail.add_piece(p);
        }
        let mut queue = all_queued(250);
        let mut order = Vec::new();
        while let Some(p) = picker.pick(&queue, &lv, top(&queue, &lv), settled(250), &avail, |_| {
            true
        }) {
            if order.len() == 6 {
                break;
            }
            order.push(p);
            queue.set(p, false);
        }
        assert_eq!(order, vec![0, 1, 2, 247, 248, 249]);
        // Then back to rarest first, i.e. not piece 3 just because it's next.
        let next = picker
            .pick(&queue, &lv, top(&queue, &lv), settled(250), &avail, |_| {
                true
            })
            .unwrap();
        assert!((3..247).contains(&next));
    }

    #[test]
    fn small_files_get_at_least_one_edge_piece() {
        let files: FileInfos = vec![file("a", 0, 1), file("b", 1, 10)];
        let mut picker = PiecePicker::new(
            PiecePicking::default(),
            lengths(11),
            &files,
            &normal_files(&files),
        );
        picker.set_options(PiecePicking {
            first_last_pieces_first: true,
            ..Default::default()
        });
        let edge = picker.compute_edge_pieces().unwrap();
        assert_eq!(edge.iter_ones().collect::<Vec<_>>(), vec![0, 1, 10]);
        picker.set_options(PiecePicking::default());
        assert!(picker.compute_edge_pieces().is_none());
    }

    // Drains the picker: the order every queued piece the peer has is picked in.
    fn drain(
        picker: &PiecePicker,
        pieces: u32,
        lv: &PieceLevels,
        avail: &PieceAvailability,
    ) -> Vec<usize> {
        let mut queue = all_queued(pieces);
        let mut order = Vec::new();
        while let Some(p) = picker.pick(&queue, lv, top(&queue, lv), settled(pieces), avail, |_| {
            true
        }) {
            order.push(p);
            queue.set(p, false);
        }
        order
    }

    fn level_of(lv: &PieceLevels, order: &[usize]) -> Vec<u8> {
        order.iter().map(|p| lv.get(*p)).collect()
    }

    #[test]
    fn higher_levels_go_first_in_every_mode() {
        // "a" (pieces 0..4) normal, "b" (4..8) high, "c" (8..12) maximum. By name "a" is walked
        // first in sequential mode and its pieces are the rarest, yet "c" then "b" go first.
        let files: FileInfos = vec![file("a", 0, 4), file("b", 4, 4), file("c", 8, 4)];
        let lv = levels(&files, 12, &[1, 6, 7], &[]);
        let avail = PieceAvailability::new(12);
        for p in 4..12 {
            avail.add_piece(p);
        }
        for sequential in [false, true] {
            for first_last_pieces_first in [false, true] {
                let picking = PiecePicking {
                    sequential,
                    first_last_pieces_first,
                };
                let picker = PiecePicker::new(picking, lengths(12), &files, &[1, 6, 7]);
                let order = drain(&picker, 12, &lv, &avail);
                assert_eq!(
                    level_of(&lv, &order),
                    [[7; 4], [6; 4], [1; 4]].concat(),
                    "{picking:?}: {order:?}"
                );
            }
        }
    }

    #[test]
    fn order_inside_a_level_is_unchanged() {
        // "b" (pieces 0..8) at maximum, "a" (8..12) normal: inside the maximum level the mode
        // orders the pieces as it does without priorities.
        let files: FileInfos = vec![file("b", 0, 8), file("a", 8, 4)];
        let lv = levels(&files, 12, &[7, 1], &[]);
        let avail = PieceAvailability::new(12);
        // Inside "b", piece 5 is the rarest, then 6, then the rest.
        for p in 0..12 {
            let copies = match p {
                5 => 1,
                6 => 2,
                _ => 3,
            };
            for _ in 0..copies {
                avail.add_piece(p);
            }
        }

        let rarest = PiecePicker::new(PiecePicking::default(), lengths(12), &files, &[7, 1]);
        let order = drain(&rarest, 12, &lv, &avail);
        assert_eq!(&order[..2], &[5, 6]);
        assert!(order[..8].iter().all(|p| *p < 8), "{order:?}");

        let sequential = PiecePicker::new(
            PiecePicking {
                sequential: true,
                ..Default::default()
            },
            lengths(12),
            &files,
            &[7, 1],
        );
        assert_eq!(
            drain(&sequential, 12, &lv, &avail),
            vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]
        );

        // 8 pieces: one edge piece at each end of "b", then rarest first; the edge pieces of
        // "a" are normal, so they wait for the maximum level to be done.
        let edges = PiecePicker::new(
            PiecePicking {
                first_last_pieces_first: true,
                ..Default::default()
            },
            lengths(12),
            &files,
            &[7, 1],
        );
        let order = drain(&edges, 12, &lv, &avail);
        assert_eq!(&order[..4], &[0, 7, 5, 6]);
        assert_eq!(&order[8..10], &[8, 11]);
    }

    #[test]
    fn piece_override_beats_the_file_level() {
        let files: FileInfos = vec![file("a", 0, 8)];
        // Piece 6 raised above its file, piece 2 skipped although its file is wanted.
        let lv = levels(&files, 8, &[1], &[(6, 7), (2, 0)]);
        let picker = PiecePicker::new(
            PiecePicking {
                sequential: true,
                ..Default::default()
            },
            lengths(8),
            &files,
            &[1],
        );
        let avail = PieceAvailability::new(8);
        assert_eq!(drain(&picker, 8, &lv, &avail), vec![6, 0, 1, 3, 4, 5, 7]);
    }

    #[test]
    fn a_piece_overridden_up_in_a_skipped_file_is_picked() {
        let files: FileInfos = vec![file("a", 0, 4), file("b", 4, 4)];
        let lv = levels(&files, 8, &[0, 1], &[(1, 1)]);
        for sequential in [false, true] {
            let picker = PiecePicker::new(
                PiecePicking {
                    sequential,
                    first_last_pieces_first: true,
                },
                lengths(8),
                &files,
                &[0, 1],
            );
            let avail = PieceAvailability::new(8);
            let mut order = drain(&picker, 8, &lv, &avail);
            order.sort_unstable();
            assert_eq!(order, vec![1, 4, 5, 6, 7]);
        }
    }

    #[test]
    fn level_zero_is_never_picked() {
        let files: FileInfos = vec![file("a", 0, 8)];
        let lv = PieceLevels::new(vec![0u8; 8].into_boxed_slice());
        for sequential in [false, true] {
            for first_last_pieces_first in [false, true] {
                let picker = PiecePicker::new(
                    PiecePicking {
                        sequential,
                        first_last_pieces_first,
                    },
                    lengths(8),
                    &files,
                    &[1],
                );
                let avail = PieceAvailability::new(8);
                // The queue says every piece is wanted; the levels say none is.
                assert_eq!(
                    picker.pick(&all_queued(8), &lv, 0, settled(8), &avail, |_| true),
                    None
                );
            }
        }
    }

    #[test]
    fn a_lower_level_is_picked_when_the_peer_lacks_the_higher_ones() {
        let files: FileInfos = vec![file("a", 0, 4), file("b", 4, 4)];
        let lv = levels(&files, 8, &[1, 7], &[]);
        let picker = PiecePicker::new(PiecePicking::default(), lengths(8), &files, &[1, 7]);
        let avail = PieceAvailability::new(8);
        let p = picker
            .pick(&all_queued(8), &lv, 7, settled(8), &avail, |p| p < 4)
            .unwrap();
        assert!(p < 4);
    }

    #[test]
    fn a_boundary_piece_keeps_its_place_in_the_wanted_file() {
        // "b" (pieces 0..6) is wanted; "a", skipped, shares b's last piece. By name "a" comes
        // first, but the shared piece is walked as the tail of "b", as without priorities.
        let files: FileInfos = vec![file("b", 0, 6), file("a", 5, 1)];
        let lv = levels(&files, 6, &[1, 0], &[]);
        let picker = PiecePicker::new(
            PiecePicking {
                sequential: true,
                ..Default::default()
            },
            lengths(6),
            &files,
            &[1, 0],
        );
        let avail = PieceAvailability::new(6);
        assert_eq!(drain(&picker, 6, &lv, &avail), vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn sequential_stops_at_the_first_piece_of_the_highest_queued_level() {
        // "a" is at maximum and done (not queued); "b" is normal. The walk stops at the first
        // piece of "b" instead of looking further for a level that is no longer queued.
        let files: FileInfos = vec![file("a", 0, 4), file("b", 4, 60)];
        let lv = levels(&files, 64, &[7, 1], &[]);
        let picker = PiecePicker::new(
            PiecePicking {
                sequential: true,
                ..Default::default()
            },
            lengths(64),
            &files,
            &[7, 1],
        );
        let avail = PieceAvailability::new(64);
        let mut queue = all_queued(64);
        queue[..4].fill(false);
        let asked = std::cell::Cell::new(0);
        let picked = picker.pick(&queue, &lv, top(&queue, &lv), settled(64), &avail, |_| {
            asked.set(asked.get() + 1);
            true
        });
        assert_eq!(picked, Some(4));
        assert_eq!(asked.get(), 1);
    }
}
