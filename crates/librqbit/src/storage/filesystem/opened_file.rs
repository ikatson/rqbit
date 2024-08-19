use std::fs::File;

use parking_lot::RwLock;

#[derive(Debug)]
pub(crate) struct OpenedFile {
    pub file: RwLock<Option<File>>,
}

impl OpenedFile {
    pub fn new(f: File) -> Self {
        Self {
            file: RwLock::new(Some(f)),
        }
    }

    pub fn take(&self) -> anyhow::Result<Option<File>> {
        let mut f = self.file.write();
        Ok(f.take())
    }

    pub fn take_clone(&self) -> anyhow::Result<Self> {
        let f = self.take()?;
        Ok(Self {
            file: RwLock::new(f),
        })
    }
}
