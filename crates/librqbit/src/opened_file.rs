use std::{
    fs::File,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::Context;
use librqbit_core::lengths::Lengths;
use parking_lot::Mutex;

#[derive(Debug)]
pub(crate) struct OpenedFile {
    pub file: Mutex<File>,
    pub filename: PathBuf,
    pub offset_in_torrent: u64,
    pub have: AtomicU64,
    pub piece_range: std::ops::Range<u32>,
    pub len: u64,
}

pub(crate) fn dummy_file() -> anyhow::Result<std::fs::File> {
    #[cfg(target_os = "windows")]
    const DEVNULL: &str = "NUL";
    #[cfg(not(target_os = "windows"))]
    const DEVNULL: &str = "/dev/null";

    std::fs::OpenOptions::new()
        .read(true)
        .open(DEVNULL)
        .with_context(|| format!("error opening {}", DEVNULL))
}

// Iterate file pieces in the following order: first, last, everything else from start to end.
fn iter_piece_priorities(range: std::ops::Range<usize>) -> impl Iterator<Item = usize> {
    // First and last of each file first, then the rest of pieces in that file.
    let r = range;
    use std::iter::once;

    let first = once(r.start);
    let last = once(r.start + r.len().overflowing_sub(1).0); // it's ok if it repeats, doesn't matter
    let mid = r.clone().skip(1).take(r.len().overflowing_sub(2).0);

    // The take(r.len()) is to not yield start/end pieces in case of 0 and 1 lengths.
    first.chain(last).chain(mid).take(r.len())
}

impl OpenedFile {
    pub fn new(
        f: File,
        filename: PathBuf,
        have: u64,
        len: u64,
        offset_in_torrent: u64,
        piece_range: std::ops::Range<u32>,
    ) -> Self {
        Self {
            file: Mutex::new(f),
            filename,
            have: AtomicU64::new(have),
            len,
            offset_in_torrent,
            piece_range,
        }
    }

    pub fn take(&self) -> anyhow::Result<File> {
        let mut f = self.file.lock();
        let dummy = dummy_file()?;
        let f = std::mem::replace(&mut *f, dummy);
        Ok(f)
    }

    pub fn take_clone(&self) -> anyhow::Result<Self> {
        let f = self.take()?;
        Ok(Self {
            file: Mutex::new(f),
            filename: self.filename.clone(),
            offset_in_torrent: self.offset_in_torrent,
            have: AtomicU64::new(self.have.load(Ordering::Relaxed)),
            len: self.len,
            piece_range: self.piece_range.clone(),
        })
    }

    pub fn piece_range_usize(&self) -> std::ops::Range<usize> {
        self.piece_range.start as usize..self.piece_range.end as usize
    }

    pub fn update_have_on_piece_completed(&self, piece_id: u32, lengths: &Lengths) -> u64 {
        let size = lengths.size_of_piece_in_file(piece_id, self.offset_in_torrent, self.len);
        self.have.fetch_add(size, Ordering::Relaxed);
        size
    }

    pub fn approx_is_finished(&self) -> bool {
        self.have.load(Ordering::Relaxed) == self.len
    }

    pub fn iter_piece_priorities(&self) -> impl Iterator<Item = usize> {
        iter_piece_priorities(self.piece_range_usize())
    }
}

#[cfg(test)]
mod tests {
    use crate::opened_file::iter_piece_priorities;

    #[test]
    fn test_iter_piece_priorities() {
        let it = |r: std::ops::Range<usize>| -> Vec<usize> { iter_piece_priorities(r).collect() };
        assert_eq!(it(0..0), Vec::<usize>::new());

        assert_eq!(it(0..1), vec![0]);
        assert_eq!(it(0..2), vec![0, 1]);
        assert_eq!(it(0..3), vec![0, 2, 1]);
        assert_eq!(it(0..4), vec![0, 3, 1, 2]);
    }
}
