use std::{
    fs::File,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::Context;
use parking_lot::RwLock;

#[derive(Debug)]
pub(crate) struct OpenedFile {
    pub filename: PathBuf,
    pub file: RwLock<File>,
    pub is_writeable: AtomicBool,
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
    pub fn new(filename: PathBuf, f: File, is_writeable: bool) -> Self {
        Self {
            filename,
            file: RwLock::new(f),
            is_writeable: AtomicBool::new(is_writeable),
        }
    }

    pub fn take(&self) -> anyhow::Result<File> {
        let mut f = self.file.write();
        let dummy = dummy_file()?;
        let f = std::mem::replace(&mut *f, dummy);
        Ok(f)
    }

    pub fn take_clone(&self) -> anyhow::Result<Self> {
        let f = self.take()?;
        Ok(Self {
            filename: self.filename.clone(),
            file: RwLock::new(f),
            is_writeable: AtomicBool::new(self.is_writeable.load(Ordering::SeqCst)),
        })
    }

    pub fn ensure_writeable(&self) -> anyhow::Result<()> {
        match self
            .is_writeable
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed)
        {
            Ok(_) => {
                // Updated, need to reopen writeable
                let mut g = self.file.write();
                let new_file = std::fs::OpenOptions::new()
                    .write(true)
                    .create(false)
                    .open(&self.filename)
                    .with_context(|| format!("error opening {:?} in write mode", self.filename))?;
                *g = new_file;
            }
            Err(_) => {
                // Didn't update, no need to reopen
            }
        }

        Ok(())
    }
}
