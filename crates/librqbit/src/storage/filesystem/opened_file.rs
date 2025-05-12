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
    pub file: RwLock<Option<File>>,
    pub is_writeable: AtomicBool,
}

impl OpenedFile {
    pub fn new(filename: PathBuf, f: File, is_writeable: bool) -> Self {
        Self {
            filename,
            file: RwLock::new(Some(f)),
            is_writeable: AtomicBool::new(is_writeable),
        }
    }

    pub fn new_dummy() -> Self {
        Self {
            filename: PathBuf::new(),
            file: RwLock::new(None),
            is_writeable: AtomicBool::new(false),
        }
    }

    pub fn take(&self) -> anyhow::Result<Option<File>> {
        let mut f = self.file.write();
        Ok(f.take())
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
                *g = Some(new_file);
            }
            Err(_) => {
                // Didn't update, no need to reopen
            }
        }

        Ok(())
    }
}
