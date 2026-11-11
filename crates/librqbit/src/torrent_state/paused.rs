use std::sync::Arc;

use crate::{
    chunk_tracker::{ChunkTracker, HaveNeededSelected},
    priority::PieceLevels,
    type_aliases::FileStorage,
};

use super::{ManagedTorrentShared, TorrentMetadata, streaming::TorrentStreams};

pub struct TorrentStatePaused {
    pub(crate) shared: Arc<ManagedTorrentShared>,
    pub(crate) metadata: Arc<TorrentMetadata>,
    pub(crate) files: FileStorage,
    pub(crate) chunk_tracker: ChunkTracker,
    pub(crate) streams: Arc<TorrentStreams>,
}

impl TorrentStatePaused {
    /// Changes the effective priority of the pieces. Nothing is in flight while paused.
    pub(crate) fn set_piece_levels(&mut self, levels: PieceLevels) -> anyhow::Result<()> {
        self.chunk_tracker.set_piece_levels(levels, |_| false)?;
        Ok(())
    }

    pub(crate) fn hns(&self) -> &HaveNeededSelected {
        self.chunk_tracker.get_hns()
    }
}
