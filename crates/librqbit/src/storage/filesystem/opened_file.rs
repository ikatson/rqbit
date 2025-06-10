use std::{
    fs::File,
    ops::{Deref, DerefMut},
    path::PathBuf,
};

use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};

#[derive(Default, Debug)]
struct OpenedFileLocked {
    #[allow(unused)]
    pub path: PathBuf,
    pub fd: Option<File>,
}

impl Deref for OpenedFileLocked {
    type Target = Option<File>;

    fn deref(&self) -> &Self::Target {
        &self.fd
    }
}

impl DerefMut for OpenedFileLocked {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.fd
    }
}

#[derive(Debug)]
pub(crate) struct OpenedFile {
    file: RwLock<OpenedFileLocked>,
}

impl OpenedFile {
    pub fn new(path: PathBuf, f: File) -> Self {
        Self {
            file: RwLock::new(OpenedFileLocked { path, fd: Some(f) }),
        }
    }

    pub fn new_dummy() -> Self {
        Self {
            file: RwLock::new(Default::default()),
        }
    }

    pub fn take_clone(&self) -> anyhow::Result<Self> {
        let f = std::mem::take(&mut *self.file.write());
        Ok(Self {
            file: RwLock::new(f),
        })
    }

    pub fn lock_read(&self) -> RwLockReadGuard<'_, impl std::ops::Deref<Target = Option<File>>> {
        self.file.read()
    }

    pub fn lock_write(
        &self,
    ) -> RwLockWriteGuard<'_, impl std::ops::DerefMut<Target = Option<File>>> {
        self.file.write()
    }
}
