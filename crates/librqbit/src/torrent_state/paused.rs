use std::{collections::HashSet, fs::File, path::PathBuf, sync::Arc};

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

impl TorrentStatePaused {
    pub(crate) fn update_only_files(&mut self, only_files: &HashSet<usize>) -> anyhow::Result<()> {
        let hn = self
            .chunk_tracker
            .update_only_files(self.info.info.iter_file_lengths()?, only_files)?;
        self.have_bytes = hn.have_bytes;
        self.needed_bytes = hn.needed_bytes;
        Ok(())
    }
}
