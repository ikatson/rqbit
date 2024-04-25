use std::{collections::HashSet, sync::Arc};

use crate::{
    chunk_tracker::{ChunkTracker, HaveNeededSelected},
    type_aliases::FileStorage,
};

use super::{streaming::TorrentStreams, ManagedTorrentInfo};

pub struct TorrentStatePaused {
    pub(crate) info: Arc<ManagedTorrentInfo>,
    pub(crate) files: FileStorage,
    pub(crate) chunk_tracker: ChunkTracker,
    pub(crate) streams: Arc<TorrentStreams>,
}

impl TorrentStatePaused {
    pub(crate) fn update_only_files(&mut self, only_files: &HashSet<usize>) -> anyhow::Result<()> {
        self.chunk_tracker
            .update_only_files(self.info.info.iter_file_lengths()?, only_files)?;
        Ok(())
    }

    pub(crate) fn hns(&self) -> &HaveNeededSelected {
        self.chunk_tracker.get_hns()
    }
}
