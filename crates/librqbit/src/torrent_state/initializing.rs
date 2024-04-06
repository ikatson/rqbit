use std::{
    fs::{File, OpenOptions},
    sync::{atomic::AtomicU64, Arc},
    time::Instant,
};

use anyhow::Context;

use size_format::SizeFormatterBinary as SF;
use tracing::{debug, info, warn};

use crate::{
    chunk_tracker::ChunkTracker, file_ops::FileOps, opened_file::OpenedFile,
    type_aliases::OpenedFiles,
};

use super::{paused::TorrentStatePaused, ManagedTorrentInfo};

fn ensure_file_length(file: &File, length: u64) -> anyhow::Result<()> {
    Ok(file.set_len(length)?)
}

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
        let mut files = OpenedFiles::new();
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
            files.push(OpenedFile::new(
                file,
                full_path,
                0,
                file_details.len,
                file_details.offset,
                file_details.pieces,
            ));
        }

        debug!("computed lengths: {:?}", &self.meta.lengths);

        info!("Doing initial checksum validation, this might take a while...");
        let initial_check_results = self.meta.spawner.spawn_block_in_place(|| {
            FileOps::new(&self.meta.info, &files, &self.meta.lengths).initial_check(
                self.only_files.as_deref(),
                &files,
                &self.meta.lengths,
                &self.checked_bytes,
            )
        })?;

        info!(
            "Initial check results: have {}, needed {}, total selected {}",
            SF::new(initial_check_results.have_bytes),
            SF::new(initial_check_results.needed_bytes),
            SF::new(initial_check_results.selected_bytes)
        );

        // Ensure file lenghts are correct, and reopen read-only.
        self.meta.spawner.spawn_block_in_place(|| {
            for (idx, file) in files.iter().enumerate() {
                if self
                    .only_files
                    .as_ref()
                    .map(|v| v.contains(&idx))
                    .unwrap_or(true)
                {
                    let now = Instant::now();
                    if let Err(err) = ensure_file_length(&file.file.lock(), file.len) {
                        warn!(
                            "Error setting length for file {:?} to {}: {:#?}",
                            file.filename, file.len, err
                        );
                    } else {
                        debug!(
                            "Set length for file {:?} to {} in {:?}",
                            file.filename,
                            SF::new(file.len),
                            now.elapsed()
                        );
                    }
                }

                file.reopen(true)?;
            }
            Ok::<_, anyhow::Error>(())
        })?;

        let chunk_tracker = ChunkTracker::new(
            initial_check_results.have_pieces,
            initial_check_results.selected_pieces,
            self.meta.lengths,
        )
        .context("error creating chunk tracker")?;

        let paused = TorrentStatePaused {
            info: self.meta.clone(),
            files,
            chunk_tracker,
        };
        Ok(paused)
    }
}
