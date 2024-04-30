pub mod example;
pub mod filesystem;

use std::path::Path;

use crate::torrent_state::ManagedTorrentInfo;

pub trait StorageFactory: Send + Sync {
    fn init_storage(&self, info: &ManagedTorrentInfo) -> anyhow::Result<Box<dyn TorrentStorage>>;

    fn output_folder(&self) -> Option<&Path> {
        None
    }
}

pub trait TorrentStorage: Send + Sync {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()>;

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()>;

    fn remove_file(&self, file_id: usize, filename: &Path) -> anyhow::Result<()>;

    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()>;

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>>;

    fn output_folder(&self) -> Option<&Path> {
        None
    }
}

impl TorrentStorage for Box<dyn TorrentStorage> {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        (**self).pread_exact(file_id, offset, buf)
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        (**self).pwrite_all(file_id, offset, buf)
    }

    fn remove_file(&self, file_id: usize, filename: &Path) -> anyhow::Result<()> {
        (**self).remove_file(file_id, filename)
    }

    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()> {
        (**self).ensure_file_length(file_id, length)
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        (**self).take()
    }
}
