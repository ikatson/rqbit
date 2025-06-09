use std::{
    fs::File,
    fs::OpenOptions,
    path::PathBuf,
};

use anyhow::Context;

use parking_lot::RwLock;

#[derive(Debug)]
pub(crate) struct FileHandle {
    pub file: File,
    pub is_writeable: bool,
}

#[derive(Debug)]
pub(crate) struct OpenedFile {
    pub filename: PathBuf,
    pub file_handle: RwLock<Option<FileHandle>>,
}

impl OpenedFile {
    pub fn new(filename: PathBuf, file: File, is_writeable: bool) -> Self {
        let file_handle = RwLock::new(Some(FileHandle {
            file,
            is_writeable,
        }));
        Self { filename, file_handle }
    }

    pub fn new_dummy() -> Self {
        Self {
            filename: PathBuf::new(),
            file_handle: None.into(),
        }
    }

    pub fn take(&self) -> anyhow::Result<Option<File>> {
        let mut fh = self.file_handle.write();
        if let Some(file_handle) = fh.take() {
            Ok(Some(file_handle.file))
        } else {
            Ok(None)
        }
    }

    pub fn is_writeable(&self) -> bool {
        self.file_handle.read().as_ref().and_then(|fh| Some(fh.is_writeable)).unwrap_or(false)
    }

    pub fn take_clone(&self) -> anyhow::Result<Self> {
        let file = self.take().unwrap().with_context(|| format!("error taking file for {:?}", self.filename))?;
        Ok(Self {
            filename: self.filename.clone(),
            file_handle: RwLock::new(
                Some(FileHandle {
                    file,
                    is_writeable: self.is_writeable(),
                })
            ),
        })
    }

    pub fn ensure_writeable(&self) -> anyhow::Result<()> {
        let mut fh = self.file_handle.write();
        if let Some(file_handle) = fh.as_mut() {
            if !file_handle.is_writeable {
                let new_file = OpenOptions::new()
                    .write(true)
                    .create(false)
                    .open(&self.filename)
                    .with_context(|| format!("error opening {:?} in write mode", self.filename))?;
                *file_handle = FileHandle {
                    file: new_file,
                    is_writeable: true,
                };
            }
        }
        Ok(())
    }
}
