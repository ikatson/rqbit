use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::Context;
use futures::{stream::FuturesUnordered, StreamExt};
use log::{debug, info, warn};
use parking_lot::{Mutex, RwLock};
use reqwest::Url;
use size_format::SizeFormatterBinary as SF;

use crate::{
    chunk_tracker::ChunkTracker,
    file_ops::FileOps,
    http_api::make_and_run_http_api,
    lengths::Lengths,
    peer_id::generate_peer_id,
    spawn_utils::{spawn, BlockingSpawner},
    speed_estimator::SpeedEstimator,
    torrent_metainfo::TorrentMetaV1Owned,
    torrent_state::{AtomicStats, TorrentState, TorrentStateLocked},
    tracker_comms::{TrackerError, TrackerRequest, TrackerRequestEvent, TrackerResponse},
    type_aliases::Sha1,
};
pub struct TorrentManagerBuilder {
    torrent: TorrentMetaV1Owned,
    overwrite: bool,
    output_folder: PathBuf,
    only_files: Option<Vec<usize>>,
    force_tracker_interval: Option<Duration>,
    spawner: Option<BlockingSpawner>,
}

impl TorrentManagerBuilder {
    pub fn new<P: AsRef<Path>>(torrent: TorrentMetaV1Owned, output_folder: P) -> Self {
        Self {
            torrent,
            overwrite: false,
            output_folder: output_folder.as_ref().into(),
            only_files: None,
            force_tracker_interval: None,
            spawner: None,
        }
    }

    pub fn only_files(&mut self, only_files: Vec<usize>) -> &mut Self {
        self.only_files = Some(only_files);
        self
    }

    pub fn overwrite(&mut self, overwrite: bool) -> &mut Self {
        self.overwrite = overwrite;
        self
    }

    pub fn force_tracker_interval(&mut self, force_tracker_interval: Duration) -> &mut Self {
        self.force_tracker_interval = Some(force_tracker_interval);
        self
    }

    pub fn spawner(&mut self, spawner: BlockingSpawner) -> &mut Self {
        self.spawner = Some(spawner);
        self
    }

    pub async fn start_manager(self) -> anyhow::Result<TorrentManagerHandle> {
        TorrentManager::start(
            self.torrent,
            self.output_folder,
            self.overwrite,
            self.only_files,
            self.force_tracker_interval,
            self.spawner.unwrap_or_else(|| BlockingSpawner::new(true)),
        )
    }
}

#[derive(Clone)]
pub struct TorrentManagerHandle {
    manager: TorrentManager,
}

impl TorrentManagerHandle {
    pub async fn cancel(&self) -> anyhow::Result<()> {
        todo!()
    }
    pub async fn wait_until_completed(&self) -> anyhow::Result<()> {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }
}

#[derive(Clone)]
struct TorrentManager {
    state: Arc<TorrentState>,
    speed_estimator: Arc<SpeedEstimator>,
    force_tracker_interval: Option<Duration>,
}

fn make_lengths(torrent: &TorrentMetaV1Owned) -> anyhow::Result<Lengths> {
    let total_length = torrent.info.iter_file_lengths().sum();
    Lengths::new(total_length, torrent.info.piece_length, None)
}

impl TorrentManager {
    pub fn start<P: AsRef<Path>>(
        torrent: TorrentMetaV1Owned,
        out: P,
        overwrite: bool,
        only_files: Option<Vec<usize>>,
        force_tracker_interval: Option<Duration>,
        spawner: BlockingSpawner,
    ) -> anyhow::Result<TorrentManagerHandle> {
        let files = {
            let mut files =
                Vec::<Arc<Mutex<File>>>::with_capacity(torrent.info.iter_file_lengths().count());

            for (path_bits, _) in torrent.info.iter_filenames_and_lengths() {
                let mut full_path = out.as_ref().to_owned();
                for bit in path_bits.iter_components() {
                    full_path.push(
                        bit.as_ref()
                            .map(|b| std::str::from_utf8(b.as_ref()))
                            .unwrap_or(Ok("output"))?,
                    );
                }

                std::fs::create_dir_all(full_path.parent().unwrap())?;
                let file = if overwrite {
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

        let peer_id = generate_peer_id();
        let lengths = make_lengths(&torrent).context("unable to compute Lengths from torrent")?;
        debug!("computed lengths: {:?}", &lengths);

        info!("Doing initial checksum validation, this might take a while...");
        let initial_check_results = FileOps::<Sha1>::new(&torrent, &files, &lengths)
            .initial_check(only_files.as_deref())?;

        info!(
            "Initial check results: have {}, needed {}",
            SF::new(initial_check_results.have_bytes),
            SF::new(initial_check_results.needed_bytes)
        );

        let chunk_tracker = ChunkTracker::new(
            initial_check_results.needed_pieces,
            initial_check_results.have_pieces,
            lengths,
        );

        let state = Arc::new(TorrentState {
            info_hash: torrent.info_hash,
            torrent,
            peer_id,
            locked: Arc::new(RwLock::new(TorrentStateLocked {
                peers: Default::default(),
                chunks: chunk_tracker,
            })),
            files,
            stats: AtomicStats {
                have: AtomicU64::new(initial_check_results.have_bytes),
                ..Default::default()
            },
            needed: initial_check_results.needed_bytes,
            lengths,
            spawner,
        });
        let estimator = Arc::new(SpeedEstimator::new(5));

        let mgr = Self {
            state,
            speed_estimator: estimator.clone(),
            force_tracker_interval,
        };

        spawn("tracker monitor", mgr.clone().task_tracker_monitor());
        spawn("stats printer", mgr.clone().stats_printer());
        spawn(
            "http api",
            make_and_run_http_api(mgr.state.clone(), estimator.clone()),
        );
        spawn("speed estimator updater", {
            let state = mgr.state.clone();
            async move {
                loop {
                    let downloaded = state.stats.downloaded_and_checked.load(Ordering::Relaxed);
                    let needed = state.needed;
                    let remaining = needed - downloaded;
                    estimator.add_snapshot(downloaded, remaining, Instant::now());
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        });

        Ok(mgr.into_handle())
    }

    async fn stats_printer(self) -> anyhow::Result<()> {
        loop {
            let live_peer_stats = self.state.locked.read().peers.stats();
            let seen_peers_count = self.state.locked.read().peers.seen().len();
            let have = self.state.stats.have.load(Ordering::Relaxed);
            let fetched = self.state.stats.fetched_bytes.load(Ordering::Relaxed);
            let needed = self.state.needed;
            let downloaded = self
                .state
                .stats
                .downloaded_and_checked
                .load(Ordering::Relaxed);
            let remaining = needed - downloaded;
            let uploaded = self.state.stats.uploaded.load(Ordering::Relaxed);
            let downloaded_pct = if downloaded == needed {
                100f64
            } else {
                (downloaded as f64 / needed as f64) * 100f64
            };
            info!(
                "Stats: downloaded {:.2}% ({:.2}), peers {{live: {}, connecting: {}, seen: {}}}, fetched {}, remaining {:.2} out of {:.2}, uploaded {:.2}, total have {:.2}",
                downloaded_pct,
                SF::new(downloaded),
                live_peer_stats.live,
                live_peer_stats.connecting,
                seen_peers_count,
                SF::new(fetched),
                SF::new(remaining),
                SF::new(needed),
                SF::new(uploaded),
                SF::new(have)
            );
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn task_tracker_monitor(self) -> anyhow::Result<()> {
        let mut seen_trackers = HashSet::new();
        let mut tracker_futures = FuturesUnordered::new();
        let parse_url = |url: &[u8]| -> anyhow::Result<Url> {
            let url = std::str::from_utf8(url).context("error parsing tracker URL")?;
            let url = Url::parse(url).context("error parsing tracker URL")?;
            Ok(url)
        };
        for tracker in self.state.torrent.iter_announce() {
            if seen_trackers.contains(&tracker) {
                continue;
            }
            seen_trackers.insert(tracker);
            let tracker_url = match parse_url(tracker) {
                Ok(url) => url,
                Err(e) => {
                    warn!("ignoring tracker: {:#}", e);
                    continue;
                }
            };
            tracker_futures.push(self.clone().single_tracker_monitor(tracker_url));
        }

        while tracker_futures.next().await.is_some() {}
        Ok(())
    }

    fn into_handle(self) -> TorrentManagerHandle {
        TorrentManagerHandle { manager: self }
    }

    async fn tracker_one_request(&self, tracker_url: Url) -> anyhow::Result<u64> {
        let response: reqwest::Response = reqwest::get(tracker_url).await?;
        if !response.status().is_success() {
            anyhow::bail!("tracker responded with {:?}", response.status());
        }
        let bytes = response.bytes().await?;
        if let Ok(error) = crate::serde_bencode_de::from_bytes::<TrackerError>(&bytes) {
            anyhow::bail!(
                "tracker returned failure. Failure reason: {}",
                error.failure_reason
            )
        };
        let response = crate::serde_bencode_de::from_bytes::<TrackerResponse>(&bytes)?;

        for peer in response.peers.iter_sockaddrs() {
            self.state.add_peer_if_not_seen(peer);
        }
        Ok(response.interval)
    }

    async fn single_tracker_monitor(self, mut tracker_url: Url) -> anyhow::Result<()> {
        let mut event = Some(TrackerRequestEvent::Started);
        loop {
            let request = TrackerRequest {
                info_hash: self.state.torrent.info_hash,
                peer_id: self.state.peer_id,
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

            let this = self.clone();
            match this.tracker_one_request(tracker_url.clone()).await {
                Ok(interval) => {
                    event = None;
                    let interval = self
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
