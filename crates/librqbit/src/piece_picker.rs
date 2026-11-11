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
    collections::HashSet,
    ops::Range,
    sync::atomic::{AtomicU32, Ordering},
};

use librqbit_core::lengths::Lengths;
use rand::RngExt;
use serde::{Deserialize, Serialize};

use crate::type_aliases::{BF, BS, FileInfos};

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

struct SelectedFile {
    pieces: Range<usize>,
    len: u64,
}

/// Picks the next piece for a peer, per the torrent's [`PiecePicking`] and selected files.
pub(crate) struct PiecePicker {
    options: PiecePicking,
    lengths: Lengths,
    // Selected, non-padding files sorted by name: the order sequential mode downloads them in.
    files: Vec<SelectedFile>,
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
    /// `only_files` None means every file is selected.
    pub fn new(
        options: PiecePicking,
        lengths: Lengths,
        file_infos: &FileInfos,
        only_files: Option<&HashSet<usize>>,
    ) -> Self {
        let mut picker = Self {
            options,
            lengths,
            files: Vec::new(),
            edge_pieces: None,
            salt: rand::rng().random(),
        };
        picker.update_only_files(file_infos, only_files);
        picker
    }

    pub fn set_options(&mut self, options: PiecePicking) {
        self.options = options;
        self.edge_pieces = self.compute_edge_pieces();
    }

    pub fn update_only_files(
        &mut self,
        file_infos: &FileInfos,
        only_files: Option<&HashSet<usize>>,
    ) {
        let mut selected: Vec<(usize, &crate::file_info::FileInfo)> = file_infos
            .iter()
            .enumerate()
            .filter(|(_, fi)| !fi.attrs.padding)
            .filter(|(id, _)| only_files.is_none_or(|o| o.contains(id)))
            .collect();
        // By name, as many torrents list their files in no particular order.
        selected.sort_by(|a, b| a.1.relative_filename.cmp(&b.1.relative_filename));
        self.files = selected
            .into_iter()
            .map(|(_, fi)| SelectedFile {
                pieces: fi.piece_range_usize(),
                len: fi.len,
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
        for f in &self.files {
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
    /// flight). `have_bytes` is how much of the torrent is verified.
    pub fn pick(
        &self,
        queue: &BS,
        have_bytes: u64,
        availability: &PieceAvailability,
        peer_has_piece: impl Fn(usize) -> bool,
    ) -> Option<usize> {
        let wanted = |p: usize| queue.get(p).is_some_and(|b| *b) && peer_has_piece(p);

        // libtorrent's top priority disregards availability, and these are few: lowest index wins.
        if let Some(edge) = &self.edge_pieces
            && let Some(p) = edge.iter_ones().find(|p| wanted(*p))
        {
            return Some(p);
        }

        if self.options.sequential {
            return self.files.iter().find_map(|f| {
                let start = f.pieces.start;
                queue
                    .get(f.pieces.clone())?
                    .iter_ones()
                    .map(|i| start + i)
                    .find(|p| peer_has_piece(*p))
            });
        }

        let random =
            have_bytes < INITIAL_RANDOM_PIECES * u64::from(self.lengths.default_piece_length());
        queue
            .iter_ones()
            .filter(|p| peer_has_piece(*p))
            .min_by_key(|p| {
                let rarity = if random { 0 } else { availability.get(*p) };
                (rarity, mix(self.salt ^ *p as u64))
            })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use librqbit_core::lengths::Lengths;

    use super::{PieceAvailability, PiecePicker, PiecePicking};
    use crate::{
        file_info::FileInfo,
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

    #[test]
    fn rarest_piece_the_peer_has_wins() {
        let files: FileInfos = vec![file("a", 0, 8)];
        let picker = PiecePicker::new(PiecePicking::default(), lengths(8), &files, None);
        let avail = PieceAvailability::new(8);
        for p in 0..8 {
            for _ in 0..(10 - p) {
                avail.add_piece(p);
            }
        }
        let queue = all_queued(8);
        // Piece 7 is the rarest overall.
        assert_eq!(picker.pick(&queue, settled(8), &avail, |_| true), Some(7));
        // The peer doesn't have 7 or 6: 5 is the rarest it has.
        assert_eq!(picker.pick(&queue, settled(8), &avail, |p| p < 6), Some(5));
        // Not queued (in flight / had) pieces are skipped.
        let mut queue = queue;
        queue.set(7, false);
        assert_eq!(picker.pick(&queue, settled(8), &avail, |_| true), Some(6));
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
        let firsts: HashSet<usize> = (0..32)
            .filter_map(|_| {
                PiecePicker::new(PiecePicking::default(), lengths(64), &files, None).pick(
                    &queue,
                    settled(64),
                    &avail,
                    |_| true,
                )
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
        let picked: HashSet<usize> = (0..32)
            .filter_map(|_| {
                PiecePicker::new(PiecePicking::default(), lengths(64), &files, None).pick(
                    &queue,
                    0,
                    &avail,
                    |_| true,
                )
            })
            .collect();
        assert!(picked.len() > 1, "initial picks followed availability");
        let picker = PiecePicker::new(PiecePicking::default(), lengths(64), &files, None);
        assert_eq!(picker.pick(&queue, settled(64), &avail, |_| true), Some(10));
    }

    #[test]
    fn sequential_walks_files_by_name() {
        // Torrent order b, a; by name a (pieces 4..8) comes first.
        let files: FileInfos = vec![file("b", 0, 4), file("a", 4, 4)];
        let picking = PiecePicking {
            sequential: true,
            ..Default::default()
        };
        let picker = PiecePicker::new(picking, lengths(8), &files, None);
        let avail = PieceAvailability::new(8);
        let mut queue = all_queued(8);
        assert_eq!(picker.pick(&queue, settled(8), &avail, |_| true), Some(4));
        queue.set(4, false);
        assert_eq!(picker.pick(&queue, settled(8), &avail, |_| true), Some(5));
        assert_eq!(picker.pick(&queue, settled(8), &avail, |p| p < 4), Some(0));
        queue[4..8].fill(false);
        assert_eq!(picker.pick(&queue, settled(8), &avail, |_| true), Some(0));
    }

    #[test]
    fn deselected_files_are_not_walked() {
        let files: FileInfos = vec![file("a", 0, 4), file("b", 4, 4)];
        let picking = PiecePicking {
            sequential: true,
            first_last_pieces_first: true,
        };
        let only: HashSet<usize> = [1].into();
        let picker = PiecePicker::new(picking, lengths(8), &files, Some(&only));
        let avail = PieceAvailability::new(8);
        // Even if the queue still holds a piece of "a" (shared boundary pieces do), it's not
        // an edge piece and not walked.
        let queue = all_queued(8);
        assert_eq!(picker.pick(&queue, settled(8), &avail, |_| true), Some(4));
        assert_eq!(picker.pick(&queue, settled(8), &avail, |p| p < 4), None);
    }

    #[test]
    fn first_and_last_pieces_go_first() {
        // 250 pieces: 1% of the file is 2.5 pieces, so 3 at each end.
        let files: FileInfos = vec![file("a", 0, 250)];
        let picking = PiecePicking {
            first_last_pieces_first: true,
            ..Default::default()
        };
        let picker = PiecePicker::new(picking, lengths(250), &files, None);
        let avail = PieceAvailability::new(250);
        // The middle is rarer, yet the ends go first.
        for p in [0, 1, 2, 247, 248, 249] {
            avail.add_piece(p);
        }
        let mut queue = all_queued(250);
        let mut order = Vec::new();
        while let Some(p) = picker.pick(&queue, settled(250), &avail, |_| true) {
            if order.len() == 6 {
                break;
            }
            order.push(p);
            queue.set(p, false);
        }
        assert_eq!(order, vec![0, 1, 2, 247, 248, 249]);
        // Then back to rarest first, i.e. not piece 3 just because it's next.
        let next = picker.pick(&queue, settled(250), &avail, |_| true).unwrap();
        assert!((3..247).contains(&next));
    }

    #[test]
    fn small_files_get_at_least_one_edge_piece() {
        let files: FileInfos = vec![file("a", 0, 1), file("b", 1, 10)];
        let mut picker = PiecePicker::new(PiecePicking::default(), lengths(11), &files, None);
        picker.set_options(PiecePicking {
            first_last_pieces_first: true,
            ..Default::default()
        });
        let edge = picker.compute_edge_pieces().unwrap();
        assert_eq!(edge.iter_ones().collect::<Vec<_>>(), vec![0, 1, 10]);
        picker.set_options(PiecePicking::default());
        assert!(picker.compute_edge_pieces().is_none());
    }
}
