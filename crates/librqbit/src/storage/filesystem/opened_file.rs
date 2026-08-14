use std::{
    fs::{File, OpenOptions},
    io::IoSlice,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
};

use anyhow::Context;
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::Error;

pub trait OurFileExt {
    fn pwrite_all_vectored(&self, offset: u64, bufs: [IoSlice<'_>; 2]) -> anyhow::Result<usize>;
    fn pread_exact(&self, offset: u64, buf: &mut [u8]) -> anyhow::Result<()>;
    fn pwrite_all(&self, offset: u64, buf: &[u8]) -> anyhow::Result<()>;
}

impl OurFileExt for File {
    #[cfg(unix)]
    fn pwrite_all_vectored(&self, offset: u64, bufs: [IoSlice<'_>; 2]) -> anyhow::Result<usize> {
        nix::sys::uio::pwritev(self, &bufs, offset.try_into()?).context("error calling pwritev")
    }

    #[cfg(not(unix))]
    fn pwrite_all_vectored(&self, offset: u64, bufs: [IoSlice<'_>; 2]) -> anyhow::Result<usize> {
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
                // assumes the message is <= CHUNK_SIZE
                use librqbit_core::constants::CHUNK_SIZE;
                let mut buf = [0u8; CHUNK_SIZE as usize];

                buf.get_mut(..l0)
                    .context("buf too small")?
                    .copy_from_slice(&bufs[0]);
                buf.get_mut(l0..l0 + l1)
                    .context("buf too small")?
                    .copy_from_slice(&bufs[1]);
                self.pwrite_all(offset, &buf[..l0 + l1])?;
                Ok(l0 + l1)
            }
        }
    }

    #[cfg(unix)]
    fn pread_exact(&self, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        use std::os::unix::fs::FileExt;

        Ok(self.read_exact_at(buf, offset)?)
    }

    #[cfg(windows)]
    fn pread_exact(&self, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        use std::os::windows::fs::FileExt;
        self.seek_read(buf, offset)?;
        Ok(())
    }

    #[cfg(not(any(windows, unix)))]
    fn pread_exact(&self, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        anyhow::bail!("pread_exact not implemented for your platform")
    }

    #[cfg(unix)]
    fn pwrite_all(&self, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        use std::os::unix::fs::FileExt;
        Ok(self.write_all_at(buf, offset)?)
    }

    #[cfg(windows)]
    fn pwrite_all(&self, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        use std::os::windows::fs::FileExt;

        let mut remaining = buf.len();
        let mut buf = buf;
        let mut offset = offset;
        while remaining > 0 {
            let written = self.seek_write(&buf[..remaining], offset)?;
            remaining -= written;
            offset += written as u64;
            buf = &buf[written..];
        }
        Ok(())
    }

    #[cfg(not(any(windows, unix)))]
    fn pwrite_all(&self, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        anyhow::bail!("pwrite_all not implemented for your platform")
    }
}

#[derive(Default, Debug)]
struct OpenedFileLocked {
    path: PathBuf,
    fd: Option<File>,
    // When `lazy`, `fd` is opened on first access (using `path` + `overwrite`)
    // instead of up front at init.
    lazy: bool,
    overwrite: bool,
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
                lazy: false,
                overwrite: false,
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

    /// A real file opened on first access instead of up front at init.
    pub fn new_lazy(path: PathBuf, overwrite: bool) -> Self {
        Self {
            file: RwLock::new(OpenedFileLocked {
                path,
                fd: None,
                lazy: true,
                overwrite,
                #[cfg(windows)]
                tried_marking_sparse: false,
            }),
        }
    }

    /// Open the backing file if it isn't open yet. A no-op for eager or padding
    /// handles and for already-open files. The open happens once, under the
    /// write lock.
    fn ensure_open(&self) -> crate::Result<()> {
        {
            let g = self.file.read();
            if g.fd.is_some() || !g.lazy {
                return Ok(());
            }
        }
        let mut g = self.file.write();
        if g.fd.is_none() && g.lazy {
            let f = open_file(&g.path, g.overwrite).map_err(Error::Anyhow)?;
            g.fd = Some(f);
        }
        Ok(())
    }

    /// The path of a lazily-openable file not yet opened (for stat-first
    /// checks). `None` once opened, or for eager/padding handles.
    pub fn lazy_unopened_path(&self) -> Option<PathBuf> {
        let g = self.file.read();
        (g.lazy && g.fd.is_none()).then(|| g.path.clone())
    }

    pub fn take_clone(&self) -> anyhow::Result<Self> {
        let f = std::mem::take(&mut *self.file.write());
        Ok(Self {
            file: RwLock::new(f),
        })
    }

    pub fn lock_read(&self) -> crate::Result<impl Deref<Target = File>> {
        self.ensure_open()?;
        RwLockReadGuard::try_map(self.file.read(), |f| f.as_ref())
            .ok()
            .ok_or(Error::FsFileIsNone)
    }

    #[allow(dead_code)]
    pub fn lock_write(&self) -> crate::Result<impl DerefMut<Target = File>> {
        self.ensure_open()?;
        RwLockWriteGuard::try_map(self.file.write(), |f| f.as_mut())
            .ok()
            .ok_or(Error::FsFileIsNone)
    }

    #[cfg(windows)]
    pub fn try_mark_sparse(&self) -> crate::Result<impl Deref<Target = File>> {
        self.ensure_open()?;
        {
            let g = self.file.read();
            if g.tried_marking_sparse {
                return RwLockReadGuard::try_map(g, |f| f.fd.as_ref())
                    .ok()
                    .ok_or(Error::FsFileIsNone);
            }
        }
        let mut g = self.file.write();
        if !g.tried_marking_sparse {
            g.tried_marking_sparse = true;
            let f = g.fd.as_ref().ok_or(Error::FsFileIsNone)?;
            tracing::debug!(path=?g.path, marked=super::sparse::mark_file_sparse(f), "marking sparse");
        }
        let g = parking_lot::RwLockWriteGuard::downgrade(g);
        Ok(RwLockReadGuard::try_map(g, |f| f.fd.as_ref()).ok().unwrap())
    }
}

/// Open (creating dirs/file as needed) a torrent file for read+write. Shared by
/// eager `init` and lazy `ensure_open` so both use identical semantics.
pub(super) fn open_file(full_path: &Path, allow_overwrite: bool) -> anyhow::Result<File> {
    std::fs::create_dir_all(full_path.parent().context("bug: no parent")?)?;
    let f = if allow_overwrite {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(full_path)
            .with_context(|| format!("error opening {full_path:?} in read/write mode"))?
    } else {
        // create_new does not seem to work with read(true), so calling this twice.
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(full_path)
            .with_context(|| {
                format!(
                    "error creating a new file (because allow_overwrite = false) {:?}",
                    full_path
                )
            })?;
        OpenOptions::new().read(true).write(true).open(full_path)?
    };
    Ok(f)
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use librqbit_core::constants::CHUNK_SIZE;
    use peer_binary_protocol::DoubleBufHelper;
    use tempfile::TempDir;

    use crate::storage::filesystem::opened_file::OurFileExt;

    #[test]
    fn test_pwrite_all_vectored() {
        let td = TempDir::with_prefix("test_pwrite_all_vectored").unwrap();
        let mut tmp_buf = [0u8; CHUNK_SIZE as usize];
        for bufsize in [10000usize, CHUNK_SIZE as usize] {
            let mut buf = vec![0u8; bufsize];
            rand::fill(&mut buf[..]);
            for split_point in [0, bufsize / 2, bufsize] {
                let path = td.path().join(format!("file_{bufsize}_{split_point}"));
                let file = std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&path)
                    .unwrap();
                let (first, second) = buf.split_at(split_point);
                let bufs = DoubleBufHelper::new(first, second).as_ioslices(bufsize);
                file.pwrite_all_vectored(0, bufs).unwrap();

                let mut file = std::fs::File::open(&path).unwrap();
                assert_eq!(file.metadata().unwrap().len(), bufsize as u64, "{path:?}");
                file.read_exact(&mut tmp_buf[..bufsize]).unwrap();
                assert_eq!(&tmp_buf[..bufsize], buf);
            }
        }
    }
}
