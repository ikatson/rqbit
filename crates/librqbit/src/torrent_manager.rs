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
use log::{debug, info, warn};
use parking_lot::Mutex;
use reqwest::Url;
use sha1w::Sha1;
use size_format::SizeFormatterBinary as SF;

use crate::{
    chunk_tracker::ChunkTracker,
    file_ops::FileOps,
    spawn_utils::{spawn, BlockingSpawner},
    torrent_state::{TorrentState, TorrentStateOptions},
    tracker_comms::{TrackerError, TrackerRequest, TrackerRequestEvent, TrackerResponse},
};

#[derive(Default)]
struct TorrentManagerOptions {
    force_tracker_interval: Option<Duration>,
    peer_connect_timeout: Option<Duration>,
    peer_read_write_timeout: Option<Duration>,
    only_files: Option<Vec<usize>>,
    peer_id: Option<Id20>,
    overwrite: bool,
}

pub struct TorrentManagerBuilder {
    info: TorrentMetaV1Info<ByteString>,
    info_hash: Id20,
    output_folder: PathBuf,
    options: TorrentManagerOptions,
    spawner: Option<BlockingSpawner>,
}

impl TorrentManagerBuilder {
    pub fn new<P: AsRef<Path>>(
        info: TorrentMetaV1Info<ByteString>,
        info_hash: Id20,
        output_folder: P,
    ) -> Self {
        Self {
            info,
            info_hash,
            output_folder: output_folder.as_ref().into(),
            spawner: None,
            options: TorrentManagerOptions::default(),
        }
    }

    pub fn only_files(&mut self, only_files: Vec<usize>) -> &mut Self {
        self.options.only_files = Some(only_files);
        self
    }

    pub fn overwrite(&mut self, overwrite: bool) -> &mut Self {
        self.options.overwrite = overwrite;
        self
    }

    pub fn force_tracker_interval(&mut self, force_tracker_interval: Duration) -> &mut Self {
        self.options.force_tracker_interval = Some(force_tracker_interval);
        self
    }

    pub fn spawner(&mut self, spawner: BlockingSpawner) -> &mut Self {
        self.spawner = Some(spawner);
        self
    }

    pub fn peer_id(&mut self, peer_id: Id20) -> &mut Self {
        self.options.peer_id = Some(peer_id);
        self
    }

    pub fn peer_connect_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.options.peer_connect_timeout = Some(timeout);
        self
    }

    pub fn peer_read_write_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.options.peer_read_write_timeout = Some(timeout);
        self
    }

    pub fn start_manager(self) -> anyhow::Result<TorrentManagerHandle> {
        TorrentManager::start(
            self.info,
            self.info_hash,
            self.output_folder,
            self.spawner.unwrap_or_else(|| BlockingSpawner::new(true)),
            Some(self.options),
        )
    }
}

#[derive(Clone)]
pub struct TorrentManagerHandle {
    manager: Arc<TorrentManager>,
}

impl TorrentManagerHandle {
    pub fn add_tracker(&self, url: Url) -> bool {
        let mgr = self.manager.clone();
        if mgr.trackers.lock().insert(url.clone()) {
            spawn(format!("tracker monitor {}", url), async move {
                mgr.single_tracker_monitor(url).await
            });
            true
        } else {
            false
        }
    }
    pub fn only_files(&self) -> Option<&[usize]> {
        self.manager.options.only_files.as_deref()
    }
    pub fn add_peer(&self, addr: SocketAddr) -> bool {
        self.manager.state.add_peer_if_not_seen(addr)
    }
    pub fn torrent_state(&self) -> &TorrentState {
        &self.manager.state
    }
    pub fn speed_estimator(&self) -> &Arc<SpeedEstimator> {
        &self.manager.speed_estimator
    }
    pub async fn cancel(&self) -> anyhow::Result<()> {
        todo!()
    }
    pub async fn wait_until_completed(&self) -> anyhow::Result<()> {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }
}

struct TorrentManager {
    state: Arc<TorrentState>,
    #[allow(dead_code)]
    speed_estimator: Arc<SpeedEstimator>,
    trackers: Mutex<HashSet<Url>>,
    options: TorrentManagerOptions,
}

fn make_lengths<ByteBuf: AsRef<[u8]>>(
    torrent: &TorrentMetaV1Info<ByteBuf>,
) -> anyhow::Result<Lengths> {
    let total_length = torrent.iter_file_lengths()?.sum();
    Lengths::new(total_length, torrent.piece_length, None)
}

fn ensure_file_length(file: &mut File, length: u64) -> anyhow::Result<()> {
    Ok(file.set_len(length)?)
}

impl TorrentManager {
    fn start<P: AsRef<Path>>(
        info: TorrentMetaV1Info<ByteString>,
        info_hash: Id20,
        out: P,
        spawner: BlockingSpawner,
        options: Option<TorrentManagerOptions>,
    ) -> anyhow::Result<TorrentManagerHandle> {
        let options = options.unwrap_or_default();
        let files = {
            let mut files =
                Vec::<Arc<Mutex<File>>>::with_capacity(info.iter_file_lengths()?.count());

            for (path_bits, _) in info.iter_filenames_and_lengths()? {
                let mut full_path = out.as_ref().to_owned();
                let relative_path = path_bits
                    .to_pathbuf()
                    .context("error converting file to path")?;
                full_path.push(relative_path);

                std::fs::create_dir_all(full_path.parent().unwrap())?;
                let file = if options.overwrite {
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
                files.push(Arc::new(Mutex::new(file)))
            }
            files
        };

        let peer_id = options.peer_id.unwrap_or_else(generate_peer_id);
        let lengths = make_lengths(&info).context("unable to compute Lengths from torrent")?;
        debug!("computed lengths: {:?}", &lengths);

        info!("Doing initial checksum validation, this might take a while...");
        let initial_check_results = spawner.spawn_block_in_place(|| {
            FileOps::<Sha1>::new(&info, &files, &lengths)
                .initial_check(options.only_files.as_deref())
        })?;

        info!(
            "Initial check results: have {}, needed {}",
            SF::new(initial_check_results.have_bytes),
            SF::new(initial_check_results.needed_bytes)
        );

        spawner.spawn_block_in_place(|| {
            for (idx, (file, (name, length))) in files
                .iter()
                .zip(info.iter_filenames_and_lengths().unwrap())
                .enumerate()
            {
                if options
                    .only_files
                    .as_ref()
                    .map(|v| !v.contains(&idx))
                    .unwrap_or(false)
                {
                    continue;
                }
                let now = Instant::now();
                if let Err(err) = ensure_file_length(&mut file.lock(), length) {
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
            peer_connect_timeout: options.peer_connect_timeout,
            peer_read_write_timeout: options.peer_read_write_timeout,
            ..Default::default()
        };

        let state = TorrentState::new(
            info,
            info_hash,
            peer_id,
            files,
            chunk_tracker,
            lengths,
            initial_check_results.have_bytes,
            initial_check_results.needed_bytes,
            spawner,
            Some(state_options),
        );

        let estimator = Arc::new(SpeedEstimator::new(5));

        let mgr = Arc::new(Self {
            state,
            speed_estimator: estimator.clone(),
            trackers: Mutex::new(HashSet::new()),
            options,
        });

        spawn("speed estimator updater", {
            let state = mgr.state.clone();
            async move {
                loop {
                    let stats = state.stats_snapshot();
                    let fetched = state.stats_snapshot().fetched_bytes;
                    let needed = state.initially_needed();
                    // fetched can be too high in theory, so for safety make sure that it doesn't wrap around u64.
                    let remaining = needed
                        .wrapping_sub(fetched)
                        .min(needed - stats.downloaded_and_checked_bytes);
                    estimator.add_snapshot(fetched, remaining, Instant::now());
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        });

        Ok(mgr.into_handle())
    }

    fn into_handle(self: Arc<Self>) -> TorrentManagerHandle {
        TorrentManagerHandle { manager: self }
    }

    async fn tracker_one_request(&self, tracker_url: Url) -> anyhow::Result<u64> {
        let response: reqwest::Response = reqwest::get(tracker_url).await?;
        if !response.status().is_success() {
            anyhow::bail!("tracker responded with {:?}", response.status());
        }
        let bytes = response.bytes().await?;
        if let Ok(error) = from_bytes::<TrackerError>(&bytes) {
            anyhow::bail!(
                "tracker returned failure. Failure reason: {}",
                error.failure_reason
            )
        };
        let response = from_bytes::<TrackerResponse>(&bytes)?;

        for peer in response.peers.iter_sockaddrs() {
            self.state.add_peer_if_not_seen(peer);
        }
        Ok(response.interval)
    }

    async fn single_tracker_monitor(&self, mut tracker_url: Url) -> anyhow::Result<()> {
        let mut event = Some(TrackerRequestEvent::Started);
        loop {
            let request = TrackerRequest {
                info_hash: self.state.info_hash(),
                peer_id: self.state.peer_id(),
                port: 6778,
                uploaded: self.state.get_uploaded(),
                downloaded: self.state.get_downloaded(),
                left: self.state.get_left_to_download(),
                compact: true,
                no_peer_id: false,
                event,
                ip: None,
                numwant: None,
                key: None,
                trackerid: None,
            };

            let request_query = request.as_querystring();
            tracker_url.set_query(Some(&request_query));

            match self.tracker_one_request(tracker_url.clone()).await {
                Ok(interval) => {
                    event = None;
                    let interval = self
                        .options
                        .force_tracker_interval
                        .unwrap_or_else(|| Duration::from_secs(interval));
                    debug!(
                        "sleeping for {:?} after calling tracker {}",
                        interval,
                        tracker_url.host().unwrap()
                    );
                    tokio::time::sleep(interval).await;
                }
                Err(e) => {
                    debug!("error calling the tracker {}: {:#}", tracker_url, e);
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
            };
        }
    }
}
