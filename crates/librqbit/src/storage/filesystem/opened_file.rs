use std::{
    fs::File,
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
        // off_t is 32 bits on 32-bit Android (and on 32-bit Linux built without
        // _FILE_OFFSET_BITS=64), so pwritev cannot address anything past 2 GiB there.
        // pwrite_all goes through pwrite64 and takes the u64 as is, so past that point
        // write the two buffers one after the other instead.
        match nix::libc::off_t::try_from(offset) {
            Ok(offset) => {
                nix::sys::uio::pwritev(self, &bufs, offset).context("error calling pwritev")
            }
            Err(_) => pwrite_all_unvectored(self, offset, bufs),
        }
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
    fn pread_exact(&self, mut offset: u64, mut buf: &mut [u8]) -> anyhow::Result<()> {
        use std::os::windows::fs::FileExt;
        while !buf.is_empty() {
            let n = self.seek_read(buf, offset)?;
            if n == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof").into());
            }
            offset += n as u64;
            buf = &mut buf[n..];
        }
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

/// What pwrite_all_vectored() falls back to when the offset does not fit the platform's
/// pwritev: the same bytes, in two plain positional writes.
#[cfg(unix)]
fn pwrite_all_unvectored(
    file: &File,
    mut offset: u64,
    bufs: [IoSlice<'_>; 2],
) -> anyhow::Result<usize> {
    for buf in &bufs {
        file.pwrite_all(offset, buf)?;
        offset += buf.len() as u64;
    }
    Ok(bufs[0].len() + bufs[1].len())
}

#[derive(Default, Debug)]
struct OpenedFileLocked {
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

    pub fn lock_read(&self) -> crate::Result<impl Deref<Target = File>> {
        RwLockReadGuard::try_map(self.file.read(), |f| f.as_ref())
            .ok()
            .ok_or(Error::FsFileIsNone)
    }

    #[allow(dead_code)]
    pub fn lock_write(&self) -> crate::Result<impl DerefMut<Target = File>> {
        RwLockWriteGuard::try_map(self.file.write(), |f| f.as_mut())
            .ok()
            .ok_or(Error::FsFileIsNone)
    }

    #[cfg(windows)]
    pub fn try_mark_sparse(&self) -> crate::Result<impl Deref<Target = File>> {
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
            let marked = super::sparse::mark_file_sparse(f);
            tracing::debug!(path=?g.path, marked, "marking sparse");
        }
        let g = parking_lot::RwLockWriteGuard::downgrade(g);
        Ok(RwLockReadGuard::try_map(g, |f| f.fd.as_ref()).ok().unwrap())
    }

    /// The length of the file on disk, or `None` if it doesn't exist (or is a padding
    /// file's dummy, which has no path).
    pub fn len(&self) -> anyhow::Result<Option<u64>> {
        let g = self.file.read();
        if let Some(fd) = g.fd.as_ref() {
            let meta = fd
                .metadata()
                .with_context(|| format!("error reading metadata of {:?}", g.path))?;
            return Ok(Some(meta.len()));
        }
        if g.path.as_os_str().is_empty() {
            return Ok(None);
        }
        match std::fs::metadata(&g.path) {
            Ok(meta) => Ok(Some(meta.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("error reading metadata of {:?}", g.path)),
        }
    }

    /// Closes the file, so that it can be renamed (Windows can't rename an open file).
    /// Returns whether it was open. Only for when nothing else uses the file.
    pub fn close(&self) -> bool {
        self.file.write().fd.take().is_some()
    }

    /// Points this file at `path` and, if `open`, opens it there for reading and writing,
    /// creating it (and its directory) if it's missing like
    /// [`FilesystemStorage::init`](super::FilesystemStorage) does. The file points at `path`
    /// even if opening it fails.
    pub fn reopen(&self, path: PathBuf, open: bool) -> anyhow::Result<()> {
        let mut g = self.file.write();
        g.fd = None;
        #[cfg(windows)]
        {
            g.tried_marking_sparse = false;
        }
        g.path = path;
        if open {
            g.fd = Some(open_read_write(&g.path)?);
        }
        Ok(())
    }

    /// Closes the file, runs `relocate` with the file's current path, and reopens the file
    /// (if it was open) at the path `relocate` returns, which becomes its path. The lock is
    /// held all along, so a concurrent read or write waits instead of finding the file
    /// closed. If `relocate` fails, the file is reopened where it was.
    pub fn with_closed(
        &self,
        relocate: impl FnOnce(&Path) -> anyhow::Result<PathBuf>,
    ) -> anyhow::Result<()> {
        let mut g = self.file.write();
        let was_open = g.fd.take().is_some();
        let result = relocate(&g.path);
        if let Ok(new_path) = &result {
            g.path = new_path.clone();
            #[cfg(windows)]
            {
                g.tried_marking_sparse = false;
            }
        }
        if was_open {
            match open_read_write(&g.path) {
                Ok(f) => g.fd = Some(f),
                Err(e) => {
                    let reopen_error = e.context(format!("error reopening {:?}", g.path));
                    return Err(match result {
                        Ok(_) => reopen_error,
                        Err(original) => original.context(format!("{reopen_error:#}")),
                    });
                }
            }
        }
        result.map(|_| ())
    }
}

// Like `FilesystemStorage::init`, creates what's missing: the user may have deleted the file
// or its directory while the torrent was loaded.
fn open_read_write(path: &Path) -> anyhow::Result<File> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("error creating directory {parent:?}"))?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("error opening {path:?} in read/write mode"))
}

#[cfg(test)]
mod tests {
    use std::io::{IoSlice, Read};

    use librqbit_core::constants::CHUNK_SIZE;
    use peer_binary_protocol::DoubleBufHelper;
    use tempfile::TempDir;

    use crate::storage::filesystem::opened_file::OurFileExt;

    // Writes every split of a random buffer at `offset` with `write`, and checks the file
    // holds exactly those bytes there.
    fn check_pwrite_all_vectored(
        name: &str,
        offset: u64,
        write: impl Fn(&std::fs::File, u64, [IoSlice<'_>; 2]) -> anyhow::Result<usize>,
    ) {
        use std::io::Seek;

        let td = TempDir::with_prefix(name).unwrap();
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
                assert_eq!(write(&file, offset, bufs).unwrap(), bufsize, "{path:?}");

                let mut file = std::fs::File::open(&path).unwrap();
                assert_eq!(
                    file.metadata().unwrap().len(),
                    offset + bufsize as u64,
                    "{path:?}"
                );
                file.seek(std::io::SeekFrom::Start(offset)).unwrap();
                file.read_exact(&mut tmp_buf[..bufsize]).unwrap();
                assert_eq!(&tmp_buf[..bufsize], buf);
            }
        }
    }

    #[test]
    fn test_pwrite_all_vectored() {
        check_pwrite_all_vectored("test_pwrite_all_vectored", 0, |f, offset, bufs| {
            f.pwrite_all_vectored(offset, bufs)
        });
    }

    // 2 GiB is where a 32-bit off_t runs out. On a 64-bit host this still goes through
    // pwritev, so it cannot reproduce the 32-bit failure; on a 32-bit target the same
    // test takes the fallback.
    //
    // Unix only, and not because the code under test is: writing at this offset leaves a
    // 2 GiB hole, which is free where files are sparse by default and is not on NTFS,
    // where Windows zero-fills it -- six files of it, more than a CI runner has room or
    // time for. Windows takes the seek_write path below, whose offset is a u64 with no
    // such boundary, so there is nothing there for this test to find.
    #[cfg(unix)]
    #[test]
    fn test_pwrite_all_vectored_past_2gib() {
        check_pwrite_all_vectored(
            "test_pwrite_all_vectored_past_2gib",
            1u64 << 31,
            |f, offset, bufs| f.pwrite_all_vectored(offset, bufs),
        );
    }

    // The fallback itself, at an offset a 32-bit off_t cannot hold: the path a 32-bit
    // Android device takes for every write past 2 GiB into a file.
    #[cfg(unix)]
    #[test]
    fn test_pwrite_all_unvectored_past_2gib() {
        check_pwrite_all_vectored(
            "test_pwrite_all_unvectored_past_2gib",
            (1u64 << 31) + 12345,
            super::pwrite_all_unvectored,
        );
    }
}
