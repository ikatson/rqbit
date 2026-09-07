// A storage that records what reached it, to check the wrappers around one against.
//
// A wrapper around a storage is transparent: anything it has no opinion about has to
// reach what it wraps. The methods that make that easy to get wrong are the ones
// [`TorrentStorage`] gives a default, because forgetting one still compiles and then
// quietly does something of its own instead of asking the storage underneath.

use std::{io::IoSlice, path::Path};

use parking_lot::Mutex;

use crate::{ManagedTorrentShared, TorrentMetadata, storage::TorrentStorage};

#[derive(Default)]
pub(crate) struct Probe {
    /// The vectored writes that arrived as one call, as (offset, total length).
    pub vectored: Mutex<Vec<(u64, usize)>>,
}

impl TorrentStorage for Probe {
    fn init(
        &mut self,
        _shared: &ManagedTorrentShared,
        _metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn pread_exact(&self, _file_id: usize, _offset: u64, _buf: &mut [u8]) -> anyhow::Result<()> {
        Ok(())
    }

    fn pwrite_all(&self, _file_id: usize, _offset: u64, _buf: &[u8]) -> anyhow::Result<()> {
        Ok(())
    }

    fn pwrite_all_vectored(
        &self,
        _file_id: usize,
        offset: u64,
        bufs: [IoSlice<'_>; 2],
    ) -> anyhow::Result<usize> {
        let len = bufs[0].len() + bufs[1].len();
        self.vectored.lock().push((offset, len));
        Ok(len)
    }

    fn remove_file(&self, _file_id: usize, _filename: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    fn remove_directory_if_empty(&self, _path: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    fn ensure_file_length(&self, _file_id: usize, _length: u64) -> anyhow::Result<()> {
        Ok(())
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        anyhow::bail!("not used")
    }
}

// A vectored write has to reach the storage underneath as one call. Called from each
// wrapper's own tests, where its private fields are.
pub(crate) fn assert_forwards_vectored_writes<S: TorrentStorage>(storage: &S, probe: &Probe) {
    let (a, b) = (&[1u8; 4][..], &[2u8; 6][..]);
    let written = storage
        .pwrite_all_vectored(0, 0, [IoSlice::new(a), IoSlice::new(b)])
        .unwrap();
    assert_eq!(written, a.len() + b.len());
    assert_eq!(
        *probe.vectored.lock(),
        vec![(0, a.len() + b.len())],
        "pwrite_all_vectored() didn't reach the storage whole: the default split it into \
         two pwrite_all()s, which is the one call this exists to avoid"
    );
}
