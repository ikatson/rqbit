use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;
use bencode::from_bytes;
use buffers::ByteString;
use librqbit_core::{
    id20::Id20, lengths::Lengths, peer_id::generate_peer_id, speed_estimator::SpeedEstimator,
    torrent_metainfo::TorrentMetaV1Info,
};
use parking_lot::Mutex;
use reqwest::Url;
use sha1w::Sha1;
use size_format::SizeFormatterBinary as SF;
use tracing::{debug, info, span, warn, Level};

use crate::{
    chunk_tracker::ChunkTracker,
    file_ops::FileOps,
    spawn_utils::{spawn, BlockingSpawner},
    torrent_state::{ManagedTorrent, ManagedTorrentHandle, TorrentStateLive, TorrentStateOptions},
    tracker_comms::{TrackerError, TrackerRequest, TrackerRequestEvent, TrackerResponse},
};

use super::{paused::TorrentStatePaused, ManagedTorrentInfo};

fn make_lengths<ByteBuf: AsRef<[u8]>>(
    torrent: &TorrentMetaV1Info<ByteBuf>,
) -> anyhow::Result<Lengths> {
    let total_length = torrent.iter_file_lengths()?.sum();
    Lengths::new(total_length, torrent.piece_length, None)
}

fn ensure_file_length(file: &File, length: u64) -> anyhow::Result<()> {
    Ok(file.set_len(length)?)
}

pub struct TorrentStateInitializing {
    info: Arc<ManagedTorrentInfo>,
    only_files: Option<Vec<usize>>,
}

impl TorrentStateInitializing {
    pub fn new(info: Arc<ManagedTorrentInfo>, only_files: Option<Vec<usize>>) -> Self {
        Self { info, only_files }
    }

    pub async fn check(&self) -> anyhow::Result<TorrentStatePaused> {
        let (files, filenames) = {
            let mut files = Vec::<Arc<Mutex<File>>>::with_capacity(
                (&self.info).info.iter_file_lengths()?.count(),
            );
            let mut filenames = Vec::new();
            for (path_bits, _) in (&self.info).info.iter_filenames_and_lengths()? {
                let mut full_path = (&self.info).out_dir.clone();
                let relative_path = path_bits
                    .to_pathbuf()
                    .context("error converting file to path")?;
                full_path.push(relative_path);

                std::fs::create_dir_all(full_path.parent().unwrap())?;
                let file = if (&self.info).options.overwrite {
                    OpenOptions::new()
                        .create(true)
                        .read(true)
                        .write(true)
                        .open(&full_path)?
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

        let lengths =
            make_lengths(&(&self.info).info).context("unable to compute Lengths from torrent")?;
        debug!("computed lengths: {:?}", &lengths);

        info!("Doing initial checksum validation, this might take a while...");
        let initial_check_results = (&self.info).spawner.spawn_block_in_place(|| {
            FileOps::<Sha1>::new(&(&self.info).info, &files, &lengths)
                .initial_check(self.only_files.as_deref())
        })?;

        info!(
            "Initial check results: have {}, needed {}",
            SF::new(initial_check_results.have_bytes),
            SF::new(initial_check_results.needed_bytes)
        );

        (&self.info).spawner.spawn_block_in_place(|| {
            for (idx, (file, (name, length))) in files
                .iter()
                .zip((&self.info).info.iter_filenames_and_lengths().unwrap())
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
            lengths,
        );

        #[allow(clippy::needless_update)]
        let state_options = TorrentStateOptions {
            peer_connect_timeout: (&self.info).options.peer_connect_timeout,
            peer_read_write_timeout: (&self.info).options.peer_read_write_timeout,
            ..Default::default()
        };

        let paused = TorrentStatePaused {
            info: self.info.clone(),
            files,
            filenames,
            chunk_tracker,
        };
        Ok(paused)
    }
}
