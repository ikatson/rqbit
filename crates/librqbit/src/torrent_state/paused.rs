use std::{fs::File, path::PathBuf, sync::Arc};

use parking_lot::Mutex;

use crate::chunk_tracker::ChunkTracker;

use super::ManagedTorrentInfo;

pub struct TorrentStatePaused {
    pub(crate) info: Arc<ManagedTorrentInfo>,
    pub(crate) files: Vec<Arc<Mutex<File>>>,
    pub(crate) filenames: Vec<PathBuf>,
    pub(crate) chunk_tracker: ChunkTracker,
    pub(crate) have_bytes: u64,
    pub(crate) needed_bytes: u64,
}

// impl TorrentStatePaused {
//     pub fn get_have_bytes(&self) -> u64 {
//         self.have_bytes
//     }
//     pub fn get_needed_bytes(&self) -> u64 {
//         self.needed_bytes
//     }
// }
