use std::{fs::File, path::PathBuf};

use anyhow::Context;
use parking_lot::Mutex;
use tracing::debug;

#[derive(Debug)]
pub(crate) struct OpenedFile {
    pub file: Mutex<File>,
    pub filename: PathBuf,
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
    pub fn new(f: File, filename: PathBuf) -> Self {
        Self {
            file: Mutex::new(f),
            filename,
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
        // this should close the original file
        // putting in a block just in case to guarantee drop.
        {
            *g = dummy_file()?;
        }
        *g = std::fs::OpenOptions::new()
            .read(true)
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
        Ok(Self::new(f, self.filename.clone()))
    }
}
