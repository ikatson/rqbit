use parking_lot::RwLock;

use super::TorrentStorage;

pub struct MemoryWatcherStorage {
    inner: Box<dyn TorrentStorage>,
    size_in_memory: RwLock<usize>,
}

impl MemoryWatcherStorage {
    pub fn new(inner: Box<dyn TorrentStorage>) -> Self {
        Self {
            inner,
            size_in_memory: RwLock::new(0),
        }
    }

    pub fn as_storage(&self) -> &dyn TorrentStorage {
        self
    }

    pub fn get_current_memory_size(&self) -> usize {
        *(self.size_in_memory.read())
    }
}

impl TorrentStorage for MemoryWatcherStorage {
    fn init(&mut self, meta: &crate::ManagedTorrentShared) -> anyhow::Result<()> {
        self.inner.init(meta)
    }

    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        self.inner.pread_exact(file_id, offset, buf)
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        {
            let mut write = self.size_in_memory.write();
            *write += buf.len();
        }

        self.inner.pwrite_all(file_id, offset, buf)
    }

    fn remove_file(&self, file_id: usize, filename: &std::path::Path) -> anyhow::Result<()> {
        self.inner.remove_file(file_id, filename)
    }

    fn remove_directory_if_empty(&self, path: &std::path::Path) -> anyhow::Result<()> {
        self.inner.remove_directory_if_empty(path)
    }

    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()> {
        self.inner.ensure_file_length(file_id, length)
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        self.inner.take()
    }
}
