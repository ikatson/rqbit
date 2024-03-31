use std::{
    fs::File,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::Context;
use librqbit_core::lengths::Lengths;
use parking_lot::Mutex;
use tracing::debug;

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
    pub fn reopen(&self, read_only: bool) -> anyhow::Result<()> {
        let log_suffix = if read_only { " read only" } else { "" };

        let mut open_opts = std::fs::OpenOptions::new();
        open_opts.read(true);
        if !read_only {
            open_opts.write(true).create(false);
        }

        let mut g = self.file.lock();
        *g = open_opts
            .open(&self.filename)
            .with_context(|| format!("error re-opening {:?}{log_suffix}", self.filename))?;
        debug!("reopened {:?}{log_suffix}", self.filename);
        Ok(())
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
}
