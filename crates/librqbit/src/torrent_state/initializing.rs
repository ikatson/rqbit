use std::{
    fs::{File, OpenOptions},
    sync::{atomic::AtomicU64, Arc},
    time::Instant,
};

use anyhow::Context;

use parking_lot::Mutex;

use sha1w::Sha1;
use size_format::SizeFormatterBinary as SF;
use tracing::{debug, info, warn};

use crate::{chunk_tracker::ChunkTracker, file_ops::FileOps};

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
        let (files, filenames) = {
            let mut files =
                Vec::<Arc<Mutex<File>>>::with_capacity(self.meta.info.iter_file_lengths()?.count());
            let mut filenames = Vec::new();
            for (path_bits, _) in self.meta.info.iter_filenames_and_lengths()? {
                let mut full_path = self.meta.out_dir.clone();
                let relative_path = path_bits
                    .to_pathbuf()
                    .context("error converting file to path")?;
                full_path.push(relative_path);

                std::fs::create_dir_all(full_path.parent().unwrap())?;
                let file = if self.meta.options.overwrite {
                    OpenOptions::new()
                        .create(true)
                        .read(true)
                        .write(true)
                        .open(&full_path)
                        .with_context(|| {
                            format!("error opening {full_path:?} in read/write mode")
                        })?
                } else {
                    // TODO: create_new does not seem to work with read(true), so calling this twice.
                    OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .open(&full_path)
                        .with_context(|| format!("error creating {:?}", &full_path))?;
                    OpenOptions::new().read(true).write(true).open(&full_path)?
                };
                filenames.push(full_path);
                files.push(Arc::new(Mutex::new(file)))
            }
            (files, filenames)
        };

        debug!("computed lengths: {:?}", &self.meta.lengths);

        info!("Doing initial checksum validation, this might take a while...");
        let initial_check_results = self.meta.spawner.spawn_block_in_place(|| {
            FileOps::<Sha1>::new(&self.meta.info, &files, &self.meta.lengths)
                .initial_check(self.only_files.as_deref(), &self.checked_bytes)
        })?;

        info!(
            "Initial check results: have {}, needed {}, total selected {}",
            SF::new(initial_check_results.have_bytes),
            SF::new(initial_check_results.needed_bytes),
            SF::new(initial_check_results.total_selected_bytes)
        );

        self.meta.spawner.spawn_block_in_place(|| {
            for (idx, (file, (name, length))) in files
                .iter()
                .zip(self.meta.info.iter_filenames_and_lengths().unwrap())
                .enumerate()
            {
                if self
                    .only_files
                    .as_ref()
                    .map(|v| !v.contains(&idx))
                    .unwrap_or(false)
                {
                    continue;
                }
                let now = Instant::now();
                if let Err(err) = ensure_file_length(&file.lock(), length) {
                    warn!(
                        "Error setting length for file {:?} to {}: {:#?}",
                        name, length, err
                    );
                } else {
                    debug!(
                        "Set length for file {:?} to {} in {:?}",
                        name,
                        SF::new(length),
                        now.elapsed()
                    );
                }
            }
        });

        let chunk_tracker = ChunkTracker::new(
            initial_check_results.needed_pieces,
            initial_check_results.have_pieces,
            self.meta.lengths,
            initial_check_results.total_selected_bytes,
        );

        let paused = TorrentStatePaused {
            info: self.meta.clone(),
            files,
            filenames,
            chunk_tracker,
            have_bytes: initial_check_results.have_bytes,
            needed_bytes: initial_check_results.needed_bytes,
        };
        Ok(paused)
    }
}
