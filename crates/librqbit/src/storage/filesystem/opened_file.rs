use std::{
    fs::File,
    io::IoSlice,
    ops::{Deref, DerefMut},
    path::PathBuf,
};

use anyhow::Context;
use parking_lot::RwLock;

#[derive(Default, Debug)]
pub(crate) struct OpenedFileLocked {
    #[allow(unused)]
    path: PathBuf,
    fd: Option<File>,
    #[cfg(windows)]
    tried_marking_sparse: bool,
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
            file: RwLock::new(OpenedFileLocked {
                path,
                fd: Some(f),
                #[cfg(windows)]
                tried_marking_sparse: false,
            }),
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

    pub fn lock_read(&self) -> impl Deref<Target = impl Deref<Target = Option<File>>> {
        self.file.read()
    }

    pub fn lock_write(&self) -> impl DerefMut<Target = impl DerefMut<Target = Option<File>>> {
        self.file.write()
    }

    #[cfg(unix)]
    pub fn pwrite_all_vectored(
        &self,
        offset: u64,
        bufs: [IoSlice<'_>; 2],
    ) -> anyhow::Result<usize> {
        let g = self.file.read();
        let fd = g.as_ref().context("empty file")?;
        nix::sys::uio::pwritev(fd, &bufs, offset.try_into()?).context("error calling pwritev")
    }

    #[cfg(not(unix))]
    pub fn pwrite_all_vectored(
        &self,
        offset: u64,
        bufs: [IoSlice<'_>; 2],
    ) -> anyhow::Result<usize> {
        match (bufs[0].len(), bufs[1].len()) {
            (len, 0) if len > 0 => {
                self.pwrite_all(offset, &bufs[0])?;
                Ok(len)
            }
            (0, len) if len > 0 => {
                self.pwrite_all(offset, &bufs[1])?;
                Ok(len)
            }
            (0, 0) => Ok(0),
            (l0, l1) => {
                // concatenate the buffers in memory so that we issue one write call instead of 2
                use librqbit_core::constants::CHUNK_SIZE;
                let mut buf = [0u8; CHUNK_SIZE as usize];

                buf[..l0].copy_from_slice(&bufs[0]);
                buf[l0..l0 + l1].copy_from_slice(&bufs[1]);
                self.pwrite_all(offset, &buf)?;
                Ok(l0 + l1)
            }
        }
    }

    pub fn pread_exact(&self, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        #[cfg(target_family = "unix")]
        {
            use std::os::unix::fs::FileExt;

            Ok(self
                .lock_read()
                .as_ref()
                .context("file is None")?
                .read_exact_at(buf, offset)?)
        }
        #[cfg(target_family = "windows")]
        {
            use std::os::windows::fs::FileExt;
            let g = self.lock_read();
            let f = g.as_ref().context("file is None")?;
            f.seek_read(buf, offset)?;
            Ok(())
        }
        #[cfg(not(any(target_family = "unix", target_family = "windows")))]
        {
            use std::io::{Read, Seek, SeekFrom};
            let mut g = self.lock_write();
            let mut f = g.as_ref().context("file is None")?;
            f.seek(SeekFrom::Start(offset))?;
            Ok(f.read_exact(buf)?)
        }
    }

    #[cfg(windows)]
    pub fn try_mark_sparse(&self) -> anyhow::Result<impl Deref<Target = OpenedFileLocked>> {
        {
            let g = self.file.read();
            if g.tried_marking_sparse {
                return Ok(g);
            }
        }
        let mut g = self.file.write();
        if !g.tried_marking_sparse {
            g.tried_marking_sparse = true;
            let f = g.fd.as_ref().context("file is None")?;
            tracing::debug!(path=?g.path, marked=super::sparse::mark_file_sparse(&f), "marking sparse");
        }
        Ok(parking_lot::RwLockWriteGuard::downgrade(g))
    }

    pub fn pwrite_all(&self, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        #[cfg(target_family = "unix")]
        {
            use std::os::unix::fs::FileExt;

            Ok(self
                .lock_read()
                .as_ref()
                .context("file is None")?
                .write_all_at(buf, offset)?)
        }
        #[cfg(target_family = "windows")]
        {
            use std::os::windows::fs::FileExt;

            let g = self.try_mark_sparse()?;
            let f = g.as_ref().context("file is None")?;
            let mut remaining = buf.len();
            let mut buf = buf;
            let mut offset = offset;
            while remaining > 0 {
                let written = f.seek_write(&buf[..remaining], offset)?;
                remaining -= written;
                offset += written as u64;
                buf = &buf[written..];
            }
            Ok(())
        }
        #[cfg(not(any(target_family = "unix", target_family = "windows")))]
        {
            use std::io::{Read, Seek, SeekFrom, Write};
            let mut g = self.lock_write();
            let mut f = g.as_ref().context("file is None")?;
            f.seek(SeekFrom::Start(offset))?;
            Ok(f.write_all(buf)?)
        }
    }
}
