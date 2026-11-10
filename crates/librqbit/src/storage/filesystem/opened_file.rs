use std::{
    fs::File,
    io::IoSlice,
    ops::Deref,
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
    path: PathBuf,
    fd: Option<File>,
    // Whether `fd` is read-only. Only Windows holds files read-only, see OpenedFile.
    read_only: bool,
}

/// A file of the torrent. Its handle is closed when the torrent is paused and opened again
/// on demand, so that a paused torrent holds no handles.
///
/// On Windows a handle with write access keeps other programs out of the file: an
/// executable that is open for writing cannot be run, and any opener that leaves out
/// FILE_SHARE_WRITE gets a sharing violation. A read-only handle (std shares read, write and
/// delete by default) blocks much less. So there a file is held read-only, and is opened for
/// writing only until it has all its pieces.
#[derive(Debug)]
pub(crate) struct OpenedFile {
    file: RwLock<OpenedFileLocked>,
}

impl OpenedFile {
    pub fn new(path: PathBuf, f: File) -> anyhow::Result<Self> {
        // On Windows `f`, opened for writing, is swapped for a read-only handle. That is done
        // here rather than on demand so that the initial check never has to open the file: an
        // error here fails the torrent, while one there would mark the file as broken.
        #[cfg(windows)]
        let f = {
            let read_only = open(&path, false)?;
            drop(f);
            read_only
        };
        Ok(Self {
            file: RwLock::new(OpenedFileLocked {
                path,
                fd: Some(f),
                read_only: cfg!(windows),
            }),
        })
    }

    pub fn new_dummy() -> Self {
        Self {
            file: RwLock::new(Default::default()),
        }
    }

    /// Moves the file to a new OpenedFile and leaves this one useless. The handle is closed
    /// rather than moved: this is what pausing a torrent goes through.
    pub fn take_clone(&self) -> anyhow::Result<Self> {
        let f = std::mem::take(&mut *self.file.write());
        Ok(Self {
            file: RwLock::new(OpenedFileLocked { fd: None, ..f }),
        })
    }

    pub fn for_read(&self) -> anyhow::Result<impl Deref<Target = File>> {
        self.handle(false)
    }

    pub fn for_write(&self) -> anyhow::Result<impl Deref<Target = File>> {
        self.handle(true)
    }

    /// Swaps a read/write handle for a read-only one once the file has all its pieces.
    #[cfg(windows)]
    pub fn reopen_read_only(&self) -> anyhow::Result<()> {
        let mut g = self.file.write();
        let replaced = if g.usable(true) { g.open(false)? } else { None };
        drop(g);
        // Closing a handle that wrote can be slow (flush, antivirus scan): not under the lock.
        drop(replaced);
        Ok(())
    }

    /// The current handle if it can do what is asked, otherwise one opened for it.
    fn handle(&self, write: bool) -> anyhow::Result<impl Deref<Target = File>> {
        let mut g = self.file.read();
        if !g.usable(write) {
            drop(g);
            let mut w = self.file.write();
            if !w.usable(write) {
                // Replaces nothing or a read-only handle, which is cheap to close here.
                w.open(write)?;
            }
            g = RwLockWriteGuard::downgrade(w);
        }
        RwLockReadGuard::try_map(g, |f| f.fd.as_ref()).map_err(|_| Error::FsFileIsNone.into())
    }
}

impl OpenedFileLocked {
    fn usable(&self, write: bool) -> bool {
        self.fd.is_some() && !(write && self.read_only)
    }

    /// Opens the file for what is asked; returns the handle this replaces, if any.
    fn open(&mut self, write: bool) -> anyhow::Result<Option<File>> {
        if self.path.as_os_str().is_empty() {
            return Err(Error::FsFileIsNone.into());
        }
        // Only Windows has a reason to hold a file read-only, elsewhere it is opened the
        // way init() opens it.
        let write = write || cfg!(not(windows));
        let f = open(&self.path, write)?;
        self.read_only = !write;
        Ok(self.fd.replace(f))
    }
}

/// The file was created by the storage init, so it has to exist.
fn open(path: &Path, write: bool) -> anyhow::Result<File> {
    let mode = if write { "read/write" } else { "read-only" };
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(write)
        .open(path)
        .with_context(|| format!("error opening {path:?} in {mode} mode"))?;
    #[cfg(windows)]
    if write {
        let marked = super::sparse::mark_file_sparse(&f);
        tracing::debug!(?path, marked, "marking sparse");
    }
    Ok(f)
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
