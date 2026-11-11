use std::{
    collections::HashSet,
    sync::{Arc, atomic::Ordering},
};

use crate::{
    chunk_tracker::{ChunkTracker, HaveNeededSelected},
    type_aliases::FileStorage,
};

use super::{
    ManagedTorrentShared, TorrentMetadata, live::stats::atomic::AtomicStats,
    streaming::TorrentStreams,
};

pub struct TorrentStatePaused {
    pub(crate) shared: Arc<ManagedTorrentShared>,
    pub(crate) metadata: Arc<TorrentMetadata>,
    pub(crate) files: FileStorage,
    pub(crate) chunk_tracker: ChunkTracker,
    pub(crate) streams: Arc<TorrentStreams>,
    // What the next live state starts its counters from.
    pub(crate) carried_stats: CarriedStats,
}

impl TorrentStatePaused {
    pub(crate) fn update_only_files(&mut self, only_files: &HashSet<usize>) -> anyhow::Result<()> {
        self.chunk_tracker
            .update_only_files(&self.metadata.file_infos, only_files)?;
        Ok(())
    }

    pub(crate) fn hns(&self) -> &HaveNeededSelected {
        self.chunk_tracker.get_hns()
    }
}

/// The transfer counters of a live torrent, kept while a storage operation
/// ([`crate::Session::force_recheck`], [`crate::Session::move_storage`],
/// [`crate::Session::rename_file`]) stops and restarts it, so that it doesn't look like a
/// fresh start. A plain pause and resume starts them from zero, as it always did.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct CarriedStats {
    pub downloaded_and_checked_bytes: u64,
    pub downloaded_and_checked_pieces: u64,
    pub uploaded_bytes: u64,
    pub fetched_bytes: u64,
    pub total_piece_download_ms: u64,
}

impl CarriedStats {
    pub(crate) fn snapshot(stats: &AtomicStats) -> Self {
        Self {
            downloaded_and_checked_bytes: stats
                .downloaded_and_checked_bytes
                .load(Ordering::Relaxed),
            downloaded_and_checked_pieces: stats
                .downloaded_and_checked_pieces
                .load(Ordering::Relaxed),
            uploaded_bytes: stats.uploaded_bytes.load(Ordering::Relaxed),
            fetched_bytes: stats.fetched_bytes.load(Ordering::Relaxed),
            total_piece_download_ms: stats.total_piece_download_ms.load(Ordering::Relaxed),
        }
    }

    /// Counters that start from these values, with `have_bytes` as given.
    pub(crate) fn to_atomic(self, have_bytes: u64) -> AtomicStats {
        let stats = AtomicStats::default();
        stats.have_bytes.store(have_bytes, Ordering::Relaxed);
        stats
            .downloaded_and_checked_bytes
            .store(self.downloaded_and_checked_bytes, Ordering::Relaxed);
        stats
            .downloaded_and_checked_pieces
            .store(self.downloaded_and_checked_pieces, Ordering::Relaxed);
        stats
            .uploaded_bytes
            .store(self.uploaded_bytes, Ordering::Relaxed);
        stats
            .fetched_bytes
            .store(self.fetched_bytes, Ordering::Relaxed);
        stats
            .total_piece_download_ms
            .store(self.total_piece_download_ms, Ordering::Relaxed);
        stats
    }
}
