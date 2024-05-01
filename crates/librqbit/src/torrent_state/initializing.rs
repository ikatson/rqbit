use std::{
    sync::{atomic::AtomicU64, Arc},
    time::Instant,
};

use anyhow::Context;

use size_format::SizeFormatterBinary as SF;
use tracing::{debug, info, warn};

use crate::{
    chunk_tracker::ChunkTracker,
    file_ops::FileOps,
    storage::{BoxStorageFactory, StorageFactory},
};

use super::{paused::TorrentStatePaused, ManagedTorrentInfo};

pub struct TorrentStateInitializing {
    pub(crate) meta: Arc<ManagedTorrentInfo>,
    pub(crate) only_files: Option<Vec<usize>>,
    pub(crate) checked_bytes: AtomicU64,
}

impl TorrentStateInitializing {
    pub fn new(meta: Arc<ManagedTorrentInfo>, only_files: Option<Vec<usize>>) -> Self {
        Self {
            meta,
            only_files,
            checked_bytes: AtomicU64::new(0),
        }
    }

    pub fn get_checked_bytes(&self) -> u64 {
        self.checked_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub async fn check(
        &self,
        storage_factory: &BoxStorageFactory,
    ) -> anyhow::Result<TorrentStatePaused> {
        let files = storage_factory.init_storage(&self.meta)?;
        info!("Doing initial checksum validation, this might take a while...");
        let initial_check_results = self.meta.spawner.spawn_block_in_place(|| {
            FileOps::new(
                &self.meta.info,
                &*files,
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
                    if let Err(err) = files.ensure_file_length(idx, fi.len) {
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
            initial_check_results.have_pieces,
            initial_check_results.selected_pieces,
            self.meta.lengths,
            &self.meta.file_infos,
        )
        .context("error creating chunk tracker")?;

        let paused = TorrentStatePaused {
            info: self.meta.clone(),
            files,
            chunk_tracker,
            streams: Arc::new(Default::default()),
        };
        Ok(paused)
    }
}
