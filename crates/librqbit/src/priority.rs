//! Download priorities of files and pieces.
//!
//! A priority is a level in `0..=7`, as in libtorrent and qBittorrent: 0 is "do not download",
//! and among the pieces that are downloaded a higher level is always asked for before a lower
//! one. qBittorrent uses 1 (normal), 6 (high) and 7 (maximum); libtorrent's default is 4. Any
//! level in range is accepted and they compare numerically.
//!
//! Every file has a level, and a piece may have an explicit one that overrides its files. The
//! effective level of a piece is its override if it has one, otherwise the highest level of the
//! files that overlap it: a piece shared by a skipped file and a wanted one is downloaded.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, bail};
use librqbit_core::lengths::Lengths;

use crate::type_aliases::{BF, FileInfos};

/// A download priority level of a file or a piece, in `0..=7`.
///
/// The public API passes levels as plain `u8`s; the constants name the levels qBittorrent uses.
///
/// ```
/// use librqbit::Priority;
/// assert_eq!(Priority::HIGH.get(), 6);
/// assert!(Priority::new(8).is_err());
/// assert_eq!(Priority::default(), Priority::NORMAL);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Priority(u8);

impl Priority {
    /// Not downloaded.
    pub const SKIP: Priority = Priority(0);
    /// The default level.
    pub const NORMAL: Priority = Priority(1);
    /// qBittorrent's "high".
    pub const HIGH: Priority = Priority(6);
    /// The highest level.
    pub const MAXIMUM: Priority = Priority(7);

    /// The level, or an error if it is above [`Priority::MAXIMUM`].
    pub fn new(level: u8) -> anyhow::Result<Self> {
        if level > Self::MAXIMUM.0 {
            bail!(
                "priority {level} is out of range, the levels are 0..={}",
                Self::MAXIMUM.0
            );
        }
        Ok(Self(level))
    }

    /// The level as a number.
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl Default for Priority {
    fn default() -> Self {
        Self::NORMAL
    }
}

impl From<Priority> for u8 {
    fn from(p: Priority) -> u8 {
        p.0
    }
}

impl TryFrom<u8> for Priority {
    type Error = anyhow::Error;

    fn try_from(level: u8) -> anyhow::Result<Self> {
        Self::new(level)
    }
}

fn validate(level: u8) -> anyhow::Result<u8> {
    Priority::new(level).map(Priority::get)
}

/// The levels of a torrent's files and its explicit piece overrides: what the user asked for.
///
/// Padding files are never downloaded and always have level 0, whatever is asked for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Priorities {
    files: Vec<u8>,
    // Bounded by the piece count: only valid piece indices are inserted.
    pieces: BTreeMap<u32, u8>,
}

impl Priorities {
    /// Every file normal.
    pub fn new(file_infos: &FileInfos) -> Self {
        Self {
            files: file_infos
                .iter()
                .map(|fi| {
                    if fi.attrs.padding {
                        Priority::SKIP.get()
                    } else {
                        Priority::NORMAL.get()
                    }
                })
                .collect(),
            pieces: BTreeMap::new(),
        }
    }

    /// The levels a torrent is added with.
    ///
    /// `file_priorities` gives a level per file index; files past its end are normal, and a
    /// longer list is an error. With `only_files` too, files outside it are 0 and the files in it
    /// take their level from `file_priorities`, where a 0 counts as normal (the file was
    /// selected). `only_files` must hold valid indices. `piece_priorities` are piece overrides.
    pub fn from_options(
        file_infos: &FileInfos,
        total_pieces: u32,
        only_files: Option<&[usize]>,
        file_priorities: Option<&[u8]>,
        piece_priorities: Option<&[(u32, u8)]>,
    ) -> anyhow::Result<Self> {
        let mut p = Self::new(file_infos);
        if let Some(levels) = file_priorities {
            if levels.len() > file_infos.len() {
                bail!(
                    "{} file priorities given, but the torrent has {} files",
                    levels.len(),
                    file_infos.len()
                );
            }
            for (idx, level) in levels.iter().copied().enumerate() {
                let level = validate(level).with_context(|| format!("priority of file {idx}"))?;
                p.set_file(file_infos, idx, level);
            }
        }
        if let Some(only_files) = only_files {
            let selected: HashSet<usize> = only_files.iter().copied().collect();
            for (idx, level) in p.files.iter_mut().enumerate() {
                if !selected.contains(&idx) {
                    *level = Priority::SKIP.get();
                } else if *level == Priority::SKIP.get() && !is_padding(file_infos, idx) {
                    *level = Priority::NORMAL.get();
                }
            }
        }
        if let Some(pieces) = piece_priorities {
            p.set_pieces(
                total_pieces,
                &pieces
                    .iter()
                    .map(|(piece, level)| (*piece, Some(*level)))
                    .collect::<Vec<_>>(),
            )?;
        }
        Ok(p)
    }

    fn set_file(&mut self, file_infos: &FileInfos, idx: usize, level: u8) {
        let padding = is_padding(file_infos, idx);
        if let Some(slot) = self.files.get_mut(idx) {
            *slot = if padding { Priority::SKIP.get() } else { level };
        }
    }

    /// One level per file.
    pub fn files(&self) -> &[u8] {
        &self.files
    }

    /// The explicit piece overrides, by piece index.
    pub fn piece_overrides(&self) -> impl Iterator<Item = (u32, u8)> + '_ {
        self.pieces.iter().map(|(p, l)| (*p, *l))
    }

    /// Changes the levels of some files. An unknown index or a level out of range is an error
    /// and changes nothing.
    pub fn set_files(
        &mut self,
        file_infos: &FileInfos,
        changes: &[(usize, u8)],
    ) -> anyhow::Result<()> {
        for (idx, level) in changes.iter().copied() {
            if idx >= self.files.len() {
                bail!(
                    "file {idx} does not exist, the torrent has {} files",
                    self.files.len()
                );
            }
            validate(level).with_context(|| format!("priority of file {idx}"))?;
        }
        for (idx, level) in changes.iter().copied() {
            self.set_file(file_infos, idx, level);
        }
        Ok(())
    }

    /// Sets (`Some`) or clears (`None`) explicit piece levels. An unknown piece or a level out
    /// of range is an error and changes nothing.
    pub fn set_pieces(
        &mut self,
        total_pieces: u32,
        changes: &[(u32, Option<u8>)],
    ) -> anyhow::Result<()> {
        for (piece, level) in changes.iter().copied() {
            if piece >= total_pieces {
                bail!("piece {piece} does not exist, the torrent has {total_pieces} pieces");
            }
            if let Some(level) = level {
                validate(level).with_context(|| format!("priority of piece {piece}"))?;
            }
        }
        for (piece, level) in changes.iter().copied() {
            match level {
                Some(level) => self.pieces.insert(piece, level),
                None => self.pieces.remove(&piece),
            };
        }
        Ok(())
    }

    /// The files with a non-zero level, or None when that is every file that is not padding.
    pub fn only_files(&self, file_infos: &FileInfos) -> Option<Vec<usize>> {
        let all = self
            .files
            .iter()
            .enumerate()
            .all(|(idx, level)| *level > 0 || is_padding(file_infos, idx));
        if all {
            return None;
        }
        Some(
            self.files
                .iter()
                .enumerate()
                .filter(|(_, level)| **level > 0)
                .map(|(idx, _)| idx)
                .collect(),
        )
    }

    /// Selects exactly `only_files`: files outside it get level 0, files in it keep their level,
    /// or become normal if they were 0. An unknown index is an error and changes nothing.
    pub fn set_only_files(
        &mut self,
        file_infos: &FileInfos,
        only_files: &HashSet<usize>,
    ) -> anyhow::Result<()> {
        if let Some(bad) = only_files.iter().find(|idx| **idx >= self.files.len()) {
            bail!("only_files contains invalid value {bad}");
        }
        for (idx, level) in self.files.iter_mut().enumerate() {
            if !only_files.contains(&idx) || is_padding(file_infos, idx) {
                *level = Priority::SKIP.get();
            } else if *level == Priority::SKIP.get() {
                *level = Priority::NORMAL.get();
            }
        }
        Ok(())
    }

    /// The effective level of every piece.
    pub fn piece_levels(&self, lengths: &Lengths, file_infos: &FileInfos) -> PieceLevels {
        let mut levels = vec![0u8; lengths.total_pieces() as usize].into_boxed_slice();
        for (fi, level) in file_infos.iter().zip(self.files.iter().copied()) {
            if fi.attrs.padding || level == 0 {
                continue;
            }
            if let Some(range) = levels.get_mut(fi.piece_range_usize()) {
                for l in range {
                    *l = (*l).max(level);
                }
            }
        }
        for (piece, level) in self.piece_overrides() {
            if let Some(l) = levels.get_mut(piece as usize) {
                *l = level;
            }
        }
        PieceLevels::new(levels)
    }
}

fn is_padding(file_infos: &FileInfos, idx: usize) -> bool {
    file_infos.get(idx).is_some_and(|fi| fi.attrs.padding)
}

/// The effective level of every piece of a torrent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PieceLevels {
    levels: Box<[u8]>,
    // Bit n is set when at least one piece has level n, for n in 1..=7.
    present: u8,
}

impl PieceLevels {
    pub fn new(levels: Box<[u8]>) -> Self {
        let present = levels
            .iter()
            .filter(|l| **l > 0)
            .fold(0u8, |acc, l| acc | (1u8 << (*l).min(7)));
        Self { levels, present }
    }

    /// How many pieces there are.
    pub fn len(&self) -> usize {
        self.levels.len()
    }

    /// The level of a piece; 0 for an index past the end.
    pub fn get(&self, piece: usize) -> u8 {
        self.levels.get(piece).copied().unwrap_or(0)
    }

    /// The highest level any piece has, 0 when nothing is wanted.
    pub fn top(&self) -> u8 {
        if self.present == 0 {
            0
        } else {
            // `present` only has bits 1..=7, so this is at most 7.
            7u8.saturating_sub(u8::try_from(self.present.leading_zeros()).unwrap_or(7))
        }
    }

    /// One level per piece.
    pub fn as_slice(&self) -> &[u8] {
        &self.levels
    }

    /// The pieces with a non-zero level.
    pub fn selected(&self, lengths: &Lengths) -> BF {
        let mut bf =
            BF::from_boxed_slice(vec![0u8; lengths.piece_bitfield_bytes()].into_boxed_slice());
        for (idx, level) in self.levels.iter().enumerate() {
            if *level > 0 {
                bf.set(idx, true);
            }
        }
        bf
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use librqbit_core::{lengths::Lengths, torrent_metainfo::FileDetailsAttrs};

    use super::{PieceLevels, Priorities, Priority};
    use crate::{file_info::FileInfo, type_aliases::FileInfos};

    const PIECE: u32 = 16384;

    // Files of the given byte lengths, back to back.
    fn files(lens: &[(u64, bool)]) -> (FileInfos, Lengths) {
        let mut offset = 0u64;
        let infos = lens
            .iter()
            .enumerate()
            .map(|(i, (len, padding))| {
                let first = u32::try_from(offset / u64::from(PIECE)).unwrap();
                let end = if *len == 0 {
                    first
                } else {
                    u32::try_from((offset + len).div_ceil(u64::from(PIECE))).unwrap()
                };
                let fi = FileInfo {
                    relative_filename: format!("f{i}").into(),
                    offset_in_torrent: offset,
                    piece_range: first..end,
                    attrs: FileDetailsAttrs {
                        padding: *padding,
                        ..Default::default()
                    },
                    len: *len,
                };
                offset += len;
                fi
            })
            .collect();
        (infos, Lengths::new(offset, PIECE).unwrap())
    }

    #[test]
    fn levels_are_validated() {
        assert_eq!(Priority::new(0).unwrap(), Priority::SKIP);
        assert_eq!(Priority::new(7).unwrap(), Priority::MAXIMUM);
        assert!(Priority::new(8).is_err());
        assert!(Priority::try_from(255).is_err());
        assert_eq!(u8::from(Priority::HIGH), 6);
    }

    #[test]
    fn piece_level_is_the_highest_overlapping_file() {
        // f0: pieces 0..=1 (half of 1), f1: pieces 1..=2.
        let (fi, l) = files(&[(PIECE as u64 + 100, false), (PIECE as u64, false)]);
        let mut p = Priorities::new(&fi);
        p.set_files(&fi, &[(0, 0), (1, 6)]).unwrap();
        assert_eq!(p.piece_levels(&l, &fi).as_slice(), &[0, 6, 6]);
        p.set_files(&fi, &[(0, 7), (1, 0)]).unwrap();
        assert_eq!(p.piece_levels(&l, &fi).as_slice(), &[7, 7, 0]);
    }

    #[test]
    fn override_beats_files_and_can_be_cleared() {
        let (fi, l) = files(&[(3 * PIECE as u64, false)]);
        let mut p = Priorities::new(&fi);
        p.set_pieces(3, &[(0, Some(0)), (2, Some(7))]).unwrap();
        assert_eq!(p.piece_levels(&l, &fi).as_slice(), &[0, 1, 7]);
        p.set_files(&fi, &[(0, 0)]).unwrap();
        assert_eq!(p.piece_levels(&l, &fi).as_slice(), &[0, 0, 7]);
        p.set_pieces(3, &[(2, None)]).unwrap();
        assert_eq!(p.piece_levels(&l, &fi).as_slice(), &[0, 0, 0]);
    }

    #[test]
    fn bad_changes_change_nothing() {
        let (fi, _) = files(&[(PIECE as u64, false), (PIECE as u64, false)]);
        let mut p = Priorities::new(&fi);
        let before = p.clone();
        assert!(p.set_files(&fi, &[(0, 7), (2, 1)]).is_err());
        assert!(p.set_files(&fi, &[(0, 7), (1, 8)]).is_err());
        assert!(p.set_pieces(2, &[(0, Some(7)), (2, Some(1))]).is_err());
        assert!(p.set_pieces(2, &[(0, Some(7)), (1, Some(9))]).is_err());
        assert!(p.set_only_files(&fi, &HashSet::from_iter([0, 5])).is_err());
        assert_eq!(p, before);
    }

    #[test]
    fn padding_files_are_always_skipped() {
        let (fi, l) = files(&[
            (100, false),
            (PIECE as u64 - 100, true),
            (PIECE as u64, false),
        ]);
        let mut p = Priorities::new(&fi);
        assert_eq!(p.files(), &[1, 0, 1]);
        p.set_files(&fi, &[(1, 7)]).unwrap();
        assert_eq!(p.files(), &[1, 0, 1]);
        // Every file that is not padding is wanted.
        assert_eq!(p.only_files(&fi), None);
        p.set_files(&fi, &[(0, 0)]).unwrap();
        assert_eq!(p.piece_levels(&l, &fi).as_slice(), &[0, 1]);
        assert_eq!(p.only_files(&fi), Some(vec![2]));
    }

    #[test]
    fn only_files_maps_to_levels() {
        let (fi, _) = files(&[(10, false), (10, false), (10, false)]);
        let mut p = Priorities::new(&fi);
        p.set_files(&fi, &[(0, 7), (1, 6)]).unwrap();
        p.set_only_files(&fi, &HashSet::from_iter([0, 2])).unwrap();
        assert_eq!(p.files(), &[7, 0, 1]);
        assert_eq!(p.only_files(&fi), Some(vec![0, 2]));
        p.set_only_files(&fi, &HashSet::from_iter([0, 1, 2]))
            .unwrap();
        // 1 was 0, so it comes back as normal.
        assert_eq!(p.files(), &[7, 1, 1]);
        assert_eq!(p.only_files(&fi), None);
    }

    #[test]
    fn add_options_combine_only_files_and_file_priorities() {
        let (fi, l) = files(&[(10, false), (10, false), (10, false), (10, false)]);
        let total = l.total_pieces();
        let p =
            Priorities::from_options(&fi, total, Some(&[0, 1, 3]), Some(&[7, 0, 6]), None).unwrap();
        // 2 is not selected; 1 is selected with level 0, so normal; 3 is past the list.
        assert_eq!(p.files(), &[7, 1, 0, 1]);
        let p = Priorities::from_options(&fi, total, None, Some(&[0, 6]), Some(&[(0, 2)])).unwrap();
        assert_eq!(p.files(), &[0, 6, 1, 1]);
        assert_eq!(p.piece_overrides().collect::<Vec<_>>(), vec![(0, 2)]);
        assert!(Priorities::from_options(&fi, total, None, Some(&[1; 5]), None).is_err());
        assert!(Priorities::from_options(&fi, total, None, Some(&[8]), None).is_err());
        assert!(Priorities::from_options(&fi, total, None, None, Some(&[(1, 1)])).is_err());
    }

    #[test]
    fn top_level() {
        assert_eq!(PieceLevels::new(vec![0, 0].into()).top(), 0);
        assert_eq!(PieceLevels::new(vec![1, 0, 1].into()).top(), 1);
        assert_eq!(PieceLevels::new(vec![1, 7, 6].into()).top(), 7);
    }
}
