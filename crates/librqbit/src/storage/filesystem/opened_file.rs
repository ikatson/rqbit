use std::{
    fs::{File, OpenOptions},
    io::IoSlice,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

use anyhow::Context;
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::Error;

static CACHED_OPEN_FDS: AtomicUsize = AtomicUsize::new(0);
const MAX_CACHED_OPEN_FDS: usize = 4096;

#[derive(Debug)]
struct TrackedFile(File);

impl TrackedFile {
    fn new(file: File) -> Self {
        CACHED_OPEN_FDS.fetch_add(1, Ordering::Relaxed);
        Self(file)
    }
}

impl Drop for TrackedFile {
    fn drop(&mut self) {
        CACHED_OPEN_FDS.fetch_sub(1, Ordering::Relaxed);
    }
}

fn open_rw_create(path: &Path) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true).truncate(false).read(true).write(true);
    match opts.open(path) {
        Ok(f) => Ok(f),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            opts.open(path)
        }
        Err(e) => Err(e),
    }
}

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
    path: PathBuf,
    fd: Option<TrackedFile>,
    read_only: bool,
    is_padding: bool,
    #[cfg(windows)]
    tried_marking_sparse: bool,
}

#[derive(Debug)]
pub(crate) struct OpenedFile {
    file: RwLock<OpenedFileLocked>,
}

impl OpenedFile {
    pub fn new_lazy(path: PathBuf) -> Self {
        Self {
            file: RwLock::new(OpenedFileLocked {
                path,
                fd: None,
                read_only: false,
                is_padding: false,
                #[cfg(windows)]
                tried_marking_sparse: false,
            }),
        }
    }

    #[allow(dead_code)]
    pub fn new(path: PathBuf, f: File) -> Self {
        Self {
            file: RwLock::new(OpenedFileLocked {
                path,
                fd: Some(TrackedFile::new(f)),
                read_only: false,
                is_padding: false,
                #[cfg(windows)]
                tried_marking_sparse: false,
            }),
        }
    }

    pub fn new_dummy() -> Self {
        Self {
            file: RwLock::new(OpenedFileLocked {
                path: PathBuf::new(),
                fd: None,
                read_only: false,
                is_padding: true,
                #[cfg(windows)]
                tried_marking_sparse: false,
            }),
        }
    }

    pub fn close(&self) {
        self.file.write().fd = None;
    }

    pub fn take_clone(&self) -> anyhow::Result<Self> {
        let f = std::mem::replace(
            &mut *self.file.write(),
            OpenedFileLocked {
                is_padding: true,
                ..Default::default()
            },
        );
        Ok(Self {
            file: RwLock::new(f),
        })
    }

    pub fn pread_exact(&self, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        {
            let g = self.file.read();
            if g.is_padding {
                return Err(Error::FsFileIsNone.into());
            }
            if let Some(tf) = &g.fd {
                return tf.0.pread_exact(offset, buf);
            }
        }
        let mut g = self.file.write();
        if g.is_padding {
            return Err(Error::FsFileIsNone.into());
        }
        if g.fd.is_none() {
            let (f, read_only) = match OpenOptions::new().read(true).write(true).open(&g.path) {
                Ok(f) => (f, false),
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    (OpenOptions::new().read(true).open(&g.path)?, true)
                }
                Err(e) => return Err(e.into()),
            };
            if CACHED_OPEN_FDS.load(Ordering::Relaxed) < MAX_CACHED_OPEN_FDS {
                g.fd = Some(TrackedFile::new(f));
                g.read_only = read_only;
            } else {
                drop(g);
                return f.pread_exact(offset, buf);
            }
        }
        let g = RwLockWriteGuard::downgrade(g);
        g.fd.as_ref()
            .ok_or(Error::FsFileIsNone)?
            .0
            .pread_exact(offset, buf)
    }

    pub fn pwrite_all(&self, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        {
            let g = self.file.read();
            if g.is_padding {
                return Err(Error::FsFileIsNone.into());
            }
            #[cfg(windows)]
            let ready = g.fd.is_some() && !g.read_only && g.tried_marking_sparse;
            #[cfg(not(windows))]
            let ready = g.fd.is_some() && !g.read_only;
            if ready && let Some(tf) = &g.fd {
                return tf.0.pwrite_all(offset, buf);
            }
        }
        let mut g = self.file.write();
        if g.is_padding {
            return Err(Error::FsFileIsNone.into());
        }
        if g.fd.is_none() || g.read_only {
            g.fd = None;
            let f = open_rw_create(&g.path)
                .with_context(|| format!("error opening {:?} in read/write mode", g.path))?;
            #[cfg(windows)]
            if !g.tried_marking_sparse {
                g.tried_marking_sparse = true;
                let marked = super::sparse::mark_file_sparse(&f);
                tracing::debug!(path=?g.path, marked, "marking sparse");
            }
            if CACHED_OPEN_FDS.load(Ordering::Relaxed) < MAX_CACHED_OPEN_FDS {
                g.fd = Some(TrackedFile::new(f));
                g.read_only = false;
            } else {
                drop(g);
                return f.pwrite_all(offset, buf);
            }
        }
        #[cfg(windows)]
        if !g.tried_marking_sparse {
            g.tried_marking_sparse = true;
            let f = &g.fd.as_ref().ok_or(Error::FsFileIsNone)?.0;
            let marked = super::sparse::mark_file_sparse(f);
            tracing::debug!(path=?g.path, marked, "marking sparse");
        }
        let g = RwLockWriteGuard::downgrade(g);
        g.fd.as_ref()
            .ok_or(Error::FsFileIsNone)?
            .0
            .pwrite_all(offset, buf)
    }

    pub fn pwrite_all_vectored(
        &self,
        offset: u64,
        bufs: [IoSlice<'_>; 2],
    ) -> anyhow::Result<usize> {
        {
            let g = self.file.read();
            if g.is_padding {
                return Err(Error::FsFileIsNone.into());
            }
            #[cfg(windows)]
            let ready = g.fd.is_some() && !g.read_only && g.tried_marking_sparse;
            #[cfg(not(windows))]
            let ready = g.fd.is_some() && !g.read_only;
            if ready && let Some(tf) = &g.fd {
                return tf.0.pwrite_all_vectored(offset, bufs);
            }
        }
        let mut g = self.file.write();
        if g.is_padding {
            return Err(Error::FsFileIsNone.into());
        }
        if g.fd.is_none() || g.read_only {
            g.fd = None;
            let f = open_rw_create(&g.path)
                .with_context(|| format!("error opening {:?} in read/write mode", g.path))?;
            #[cfg(windows)]
            if !g.tried_marking_sparse {
                g.tried_marking_sparse = true;
                let marked = super::sparse::mark_file_sparse(&f);
                tracing::debug!(path=?g.path, marked, "marking sparse");
            }
            if CACHED_OPEN_FDS.load(Ordering::Relaxed) < MAX_CACHED_OPEN_FDS {
                g.fd = Some(TrackedFile::new(f));
                g.read_only = false;
            } else {
                drop(g);
                return f.pwrite_all_vectored(offset, bufs);
            }
        }
        #[cfg(windows)]
        if !g.tried_marking_sparse {
            g.tried_marking_sparse = true;
            let f = &g.fd.as_ref().ok_or(Error::FsFileIsNone)?.0;
            let marked = super::sparse::mark_file_sparse(f);
            tracing::debug!(path=?g.path, marked, "marking sparse");
        }
        let g = RwLockWriteGuard::downgrade(g);
        g.fd.as_ref()
            .ok_or(Error::FsFileIsNone)?
            .0
            .pwrite_all_vectored(offset, bufs)
    }

    pub fn ensure_file_length(&self, len: u64) -> anyhow::Result<()> {
        let mut g = self.file.write();
        if g.is_padding {
            return Err(Error::FsFileIsNone.into());
        }
        if g.fd.is_some() && !g.read_only {
            #[cfg(windows)]
            if !g.tried_marking_sparse {
                g.tried_marking_sparse = true;
                let f = &g.fd.as_ref().ok_or(Error::FsFileIsNone)?.0;
                let marked = super::sparse::mark_file_sparse(f);
                tracing::debug!(path=?g.path, marked, "marking sparse");
            }
            if let Some(tf) = &g.fd {
                tf.0.set_len(len)?;
            }
            return Ok(());
        }
        g.fd = None;
        let f = open_rw_create(&g.path)
            .with_context(|| format!("error opening {:?} in read/write mode", g.path))?;
        #[cfg(windows)]
        if !g.tried_marking_sparse {
            g.tried_marking_sparse = true;
            let marked = super::sparse::mark_file_sparse(&f);
            tracing::debug!(path=?g.path, marked, "marking sparse");
        }
        f.set_len(len)?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn lock_read(&self) -> anyhow::Result<impl Deref<Target = File>> {
        {
            let g = self.file.read();
            if g.is_padding {
                return Err(Error::FsFileIsNone.into());
            }
            if g.fd.is_some() {
                return RwLockReadGuard::try_map(g, |f| f.fd.as_ref().map(|tf| &tf.0))
                    .ok()
                    .ok_or_else(|| Error::FsFileIsNone.into());
            }
        }
        let mut g = self.file.write();
        if g.is_padding {
            return Err(Error::FsFileIsNone.into());
        }
        if g.fd.is_none() {
            let f = open_rw_create(&g.path)?;
            g.fd = Some(TrackedFile::new(f));
            g.read_only = false;
        }
        let g = RwLockWriteGuard::downgrade(g);
        RwLockReadGuard::try_map(g, |f| f.fd.as_ref().map(|tf| &tf.0))
            .ok()
            .ok_or_else(|| Error::FsFileIsNone.into())
    }

    #[allow(dead_code)]
    pub fn lock_write(&self) -> anyhow::Result<impl DerefMut<Target = File>> {
        let mut g = self.file.write();
        if g.is_padding {
            return Err(Error::FsFileIsNone.into());
        }
        if g.fd.is_none() || g.read_only {
            g.fd = None;
            let f = open_rw_create(&g.path)?;
            g.fd = Some(TrackedFile::new(f));
            g.read_only = false;
        }
        RwLockWriteGuard::try_map(g, |f| f.fd.as_mut().map(|tf| &mut tf.0))
            .ok()
            .ok_or_else(|| Error::FsFileIsNone.into())
    }
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

    #[test]
    fn test_opened_file_lazy_and_fd_bounded() {
        use super::{CACHED_OPEN_FDS, MAX_CACHED_OPEN_FDS, OpenedFile};
        use std::sync::atomic::Ordering;

        let td = TempDir::with_prefix("test_lazy_opened_file").unwrap();
        let nested = td.path().join("sub/dir/file.bin");

        let of = OpenedFile::new_lazy(nested.clone());
        let mut buf = [0u8; 4];
        assert!(of.pread_exact(0, &mut buf).is_err());
        assert!(!nested.exists());

        of.ensure_file_length(16).unwrap();
        assert!(nested.exists());
        assert_eq!(nested.metadata().unwrap().len(), 16);
        assert!(of.file.read().fd.is_none());

        of.pwrite_all(0, b"abcd").unwrap();
        of.pread_exact(0, &mut buf).unwrap();
        assert_eq!(&buf, b"abcd");

        let taken = of.take_clone().unwrap();
        assert!(matches!(
            of.pwrite_all(0, b"wxyz")
                .unwrap_err()
                .downcast_ref::<crate::Error>(),
            Some(crate::Error::FsFileIsNone)
        ));
        assert!(matches!(
            of.pread_exact(0, &mut buf)
                .unwrap_err()
                .downcast_ref::<crate::Error>(),
            Some(crate::Error::FsFileIsNone)
        ));
        assert!(matches!(
            of.ensure_file_length(32)
                .unwrap_err()
                .downcast_ref::<crate::Error>(),
            Some(crate::Error::FsFileIsNone)
        ));
        taken.pread_exact(0, &mut buf).unwrap();
        assert_eq!(&buf, b"abcd");
        drop(of);
        drop(taken);

        let files: Vec<OpenedFile> = (0..(MAX_CACHED_OPEN_FDS + 100))
            .map(|i| {
                let p = td.path().join(format!("f_{i}.bin"));
                let f = OpenedFile::new_lazy(p);
                f.pwrite_all(0, b"x").unwrap();
                f
            })
            .collect();
        let cached_in_vec = files.iter().filter(|f| f.file.read().fd.is_some()).count();
        assert!(cached_in_vec <= MAX_CACHED_OPEN_FDS);
        assert!(CACHED_OPEN_FDS.load(Ordering::Relaxed) <= MAX_CACHED_OPEN_FDS + 64);
        for f in &files {
            f.close();
        }
        assert_eq!(
            files.iter().filter(|f| f.file.read().fd.is_some()).count(),
            0
        );
    }
}
