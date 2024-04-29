use std::{
    fs::OpenOptions,
    sync::{atomic::AtomicU64, Arc},
    time::Instant,
};

use anyhow::Context;

use size_format::SizeFormatterBinary as SF;
use tracing::{debug, info, warn};

use crate::{
    chunk_tracker::ChunkTracker,
    file_ops::FileOps,
    opened_file::OpenedFile,
    storage::{FilesystemStorage, InMemoryGarbageCollectingStorage, TorrentStorage},
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

    pub async fn check(&self) -> anyhow::Result<TorrentStatePaused> {
        // Return in-memory store
        let store =
            InMemoryGarbageCollectingStorage::new(self.meta.lengths, self.meta.file_infos.clone())?;
        let ct = ChunkTracker::new_empty(self.meta.lengths, &self.meta.file_infos)?;

        Ok(TorrentStatePaused {
            info: self.meta.clone(),
            files: Box::new(store),
            chunk_tracker: ct,
            streams: Arc::new(Default::default()),
        })

        // self.check_disk().await
    }

    pub async fn check_disk(&self) -> anyhow::Result<TorrentStatePaused> {
        let mut files = Vec::<OpenedFile>::new();
        for file_details in self.meta.info.iter_file_details(&self.meta.lengths)? {
            let mut full_path = self.meta.out_dir.clone();
            let relative_path = file_details
                .filename
                .to_pathbuf()
                .context("error converting file to path")?;
            full_path.push(relative_path);

            std::fs::create_dir_all(full_path.parent().context("bug: no parent")?)?;
            let file = if self.meta.options.overwrite {
                OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(&full_path)
                    .with_context(|| format!("error opening {full_path:?} in read/write mode"))?
            } else {
                // TODO: create_new does not seem to work with read(true), so calling this twice.
                OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&full_path)
                    .with_context(|| format!("error creating {:?}", &full_path))?;
                OpenOptions::new().read(true).write(true).open(&full_path)?
            };
            files.push(OpenedFile::new(file));
        }
        let files: Box<dyn TorrentStorage> = Box::new(FilesystemStorage::new(files));

        debug!("computed lengths: {:?}", &self.meta.lengths);

        info!("Doing initial checksum validation, this might take a while...");
        let initial_check_results = self.meta.spawner.spawn_block_in_place(|| {
            FileOps::new(
                &self.meta.info,
                &files,
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
                            fi.filename, fi.len, err
                        );
                    } else {
                        debug!(
                            "Set length for file {:?} to {} in {:?}",
                            fi.filename,
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
