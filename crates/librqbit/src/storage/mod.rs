pub mod example;
pub mod filesystem;
pub mod mmap;
pub mod slow;

use std::{any::Any, path::Path};

use crate::torrent_state::ManagedTorrentInfo;

pub trait StorageFactory: Send + Sync + Any {
    fn init_storage(&self, info: &ManagedTorrentInfo) -> anyhow::Result<Box<dyn TorrentStorage>>;
}

pub trait TorrentStorage: Send + Sync {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()>;

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()>;

    fn remove_file(&self, file_id: usize, filename: &Path) -> anyhow::Result<()>;

    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()>;

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>>;
}
