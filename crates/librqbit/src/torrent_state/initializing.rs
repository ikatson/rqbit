use std::{
    sync::{atomic::AtomicU64, Arc},
    time::Instant,
};

use anyhow::Context;

use size_format::SizeFormatterBinary as SF;
use tracing::{debug, info, warn};

use crate::{
    bitv::BitV, chunk_tracker::ChunkTracker, file_ops::FileOps, type_aliases::FileStorage,
};

use super::{paused::TorrentStatePaused, ManagedTorrentInfo};

pub struct TorrentStateInitializing {
    pub(crate) files: FileStorage,
    pub(crate) meta: Arc<ManagedTorrentInfo>,
    pub(crate) only_files: Option<Vec<usize>>,
    pub(crate) checked_bytes: AtomicU64,
}

impl TorrentStateInitializing {
    pub fn new(
        meta: Arc<ManagedTorrentInfo>,
        only_files: Option<Vec<usize>>,
        files: FileStorage,
    ) -> Self {
        Self {
            meta,
            only_files,
            files,
            checked_bytes: AtomicU64::new(0),
        }
    }

    pub fn get_checked_bytes(&self) -> u64 {
        self.checked_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub async fn check(&self) -> anyhow::Result<TorrentStatePaused> {
        info!("Doing initial checksum validation, this might take a while...");
        let initial_check_results = self.meta.spawner.spawn_block_in_place(|| {
            FileOps::new(
                &self.meta.info,
                &self.files,
                &self.meta.file_infos,
                &self.meta.lengths,
            )
            .initial_check(self.only_files.as_deref(), &self.checked_bytes)
        })?;

        info!(
            "Initial check results: have {}, needed {}, total selected {}",
            SF::new(initial_check_results.have_bytes),
            SF::new(initial_check_results.needed_bytes),
            SF::new(initial_check_results.selected_bytes)
        );

        // Ensure file lenghts are correct, and reopen read-only.
        self.meta.spawner.spawn_block_in_place(|| {
            for (idx, fi) in self.meta.file_infos.iter().enumerate() {
                if self
                    .only_files
                    .as_ref()
                    .map(|v| v.contains(&idx))
                    .unwrap_or(true)
                {
                    let now = Instant::now();
                    if let Err(err) = self.files.ensure_file_length(idx, fi.len) {
                        warn!(
                            "Error setting length for file {:?} to {}: {:#?}",
                            fi.relative_filename, fi.len, err
                        );
                    } else {
                        debug!(
                            "Set length for file {:?} to {} in {:?}",
                            fi.relative_filename,
                            SF::new(fi.len),
                            now.elapsed()
                        );
                    }
                }
            }
            Ok::<_, anyhow::Error>(())
        })?;

        let chunk_tracker = ChunkTracker::new(
            initial_check_results.have_pieces.into_dyn(),
            initial_check_results.selected_pieces,
            self.meta.lengths,
            &self.meta.file_infos,
        )
        .context("error creating chunk tracker")?;

        let paused = TorrentStatePaused {
            info: self.meta.clone(),
            files: self.files.take()?,
            chunk_tracker,
            streams: Arc::new(Default::default()),
        };
        Ok(paused)
    }
}
