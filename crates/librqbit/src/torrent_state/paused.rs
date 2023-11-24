use std::{fs::File, path::PathBuf, sync::Arc};

use parking_lot::Mutex;

use crate::chunk_tracker::ChunkTracker;

use super::ManagedTorrentInfo;

pub struct TorrentStatePaused {
    pub(crate) info: Arc<ManagedTorrentInfo>,
    pub(crate) files: Vec<Arc<Mutex<File>>>,
    pub(crate) filenames: Vec<PathBuf>,
    pub(crate) chunk_tracker: ChunkTracker,
}
