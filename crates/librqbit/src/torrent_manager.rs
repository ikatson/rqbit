use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    net::SocketAddr,
    ops::Deref,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::Context;
use bencode::from_bytes;
use buffers::ByteString;
use librqbit_core::{
    lengths::Lengths, peer_id::generate_peer_id, speed_estimator::SpeedEstimator,
    torrent_metainfo::TorrentMetaV1Info,
};
use log::{debug, info};
use parking_lot::{Mutex, RwLock};
use reqwest::Url;
use sha1w::Sha1;
use size_format::SizeFormatterBinary as SF;

use crate::{
    chunk_tracker::ChunkTracker,
    file_ops::FileOps,
    http_api::make_and_run_http_api,
    spawn_utils::{spawn, BlockingSpawner},
    torrent_state::{AtomicStats, TorrentState, TorrentStateLocked},
    tracker_comms::{TrackerError, TrackerRequest, TrackerRequestEvent, TrackerResponse},
};
pub struct TorrentManagerBuilder {
    info: TorrentMetaV1Info<ByteString>,
    info_hash: [u8; 20],
    overwrite: bool,
    output_folder: PathBuf,
    only_files: Option<Vec<usize>>,
    peer_id: Option<[u8; 20]>,
    force_tracker_interval: Option<Duration>,
    spawner: Option<BlockingSpawner>,
}

impl TorrentManagerBuilder {
    pub fn new<P: AsRef<Path>>(
        info: TorrentMetaV1Info<ByteString>,
        info_hash: [u8; 20],
        output_folder: P,
    ) -> Self {
        Self {
            info,
            info_hash,
            overwrite: false,
            output_folder: output_folder.as_ref().into(),
            only_files: None,
            peer_id: None,
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

    pub fn peer_id(&mut self, peer_id: [u8; 20]) -> &mut Self {
        self.peer_id = Some(peer_id);
        self
    }

    pub fn start_manager(self) -> anyhow::Result<TorrentManagerHandle> {
        TorrentManager::start(
            self.info,
            self.info_hash,
            self.output_folder,
            self.overwrite,
            self.only_files,
            self.force_tracker_interval,
            self.peer_id,
            self.spawner.unwrap_or_else(|| BlockingSpawner::new(true)),
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
    pub fn add_peer(&self, addr: SocketAddr) -> bool {
        self.manager.state.add_peer_if_not_seen(addr)
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
    force_tracker_interval: Option<Duration>,
}

fn make_lengths<ByteBuf: Clone + Deref<Target = [u8]>>(
    torrent: &TorrentMetaV1Info<ByteBuf>,
) -> anyhow::Result<Lengths> {
    let total_length = torrent.iter_file_lengths().sum();
    Lengths::new(total_length, torrent.piece_length, None)
}

impl TorrentManager {
    #[allow(clippy::too_many_arguments)]
    fn start<P: AsRef<Path>>(
        info: TorrentMetaV1Info<ByteString>,
        info_hash: [u8; 20],
        out: P,
        overwrite: bool,
        only_files: Option<Vec<usize>>,
        force_tracker_interval: Option<Duration>,
        peer_id: Option<[u8; 20]>,
        spawner: BlockingSpawner,
    ) -> anyhow::Result<TorrentManagerHandle> {
        let files = {
            let mut files =
                Vec::<Arc<Mutex<File>>>::with_capacity(info.iter_file_lengths().count());

            for (path_bits, _) in info.iter_filenames_and_lengths() {
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

        let peer_id = peer_id.unwrap_or_else(generate_peer_id);
        let lengths = make_lengths(&info).context("unable to compute Lengths from torrent")?;
        debug!("computed lengths: {:?}", &lengths);

        info!("Doing initial checksum validation, this might take a while...");
        let initial_check_results =
            FileOps::<Sha1>::new(&info, &files, &lengths).initial_check(only_files.as_deref())?;

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
            info_hash,
            torrent: info,
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

        let mgr = Arc::new(Self {
            state,
            speed_estimator: estimator.clone(),
            trackers: Mutex::new(HashSet::new()),
            force_tracker_interval,
        });

        spawn("stats printer", {
            let this = mgr.clone();
            async move { this.stats_printer().await }
        });
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

    async fn stats_printer(&self) -> anyhow::Result<()> {
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
                info_hash: self.state.info_hash,
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

            match self.tracker_one_request(tracker_url.clone()).await {
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
