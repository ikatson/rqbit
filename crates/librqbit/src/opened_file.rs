use std::fs::File;

use anyhow::Context;
use parking_lot::Mutex;

use crate::file_info::FileInfo;

#[derive(Debug)]
pub(crate) struct OpenedFile {
    pub file: Mutex<File>,
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
    pub fn new(f: File) -> Self {
        Self {
            file: Mutex::new(f),
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
        })
    }
}
