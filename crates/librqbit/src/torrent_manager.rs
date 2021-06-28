use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::Context;
use futures::{stream::FuturesUnordered, StreamExt};
use log::{debug, error, info, warn};
use parking_lot::{Mutex, RwLock};
use reqwest::Url;
use size_format::SizeFormatterBinary as SF;

use crate::{
    chunk_tracker::ChunkTracker,
    file_ops::FileOps,
    lengths::Lengths,
    peer_binary_protocol::MessageOwned,
    peer_connection::{PeerConnection, WriterRequest},
    spawn_utils::spawn,
    torrent_metainfo::TorrentMetaV1Owned,
    torrent_state::{AtomicStats, TorrentState, TorrentStateLocked},
    tracker_comms::{CompactTrackerResponse, TrackerRequest, TrackerRequestEvent},
};
pub struct TorrentManagerBuilder {
    torrent: TorrentMetaV1Owned,
    overwrite: bool,
    output_folder: PathBuf,
    only_files: Option<Vec<usize>>,
}

impl TorrentManagerBuilder {
    pub fn new<P: AsRef<Path>>(torrent: TorrentMetaV1Owned, output_folder: P) -> Self {
        Self {
            torrent,
            overwrite: false,
            output_folder: output_folder.as_ref().into(),
            only_files: None,
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

    pub async fn start_manager(self) -> anyhow::Result<TorrentManagerHandle> {
        TorrentManager::start(
            self.torrent,
            self.output_folder,
            self.overwrite,
            self.only_files,
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
    inner: Arc<TorrentState>,
}

fn generate_peer_id() -> [u8; 20] {
    let mut peer_id = [0u8; 20];
    let u = uuid::Uuid::new_v4();
    (&mut peer_id[..16]).copy_from_slice(&u.as_bytes()[..]);
    peer_id
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
        let initial_check_results =
            FileOps::new(&torrent, &files, &lengths).initial_check(only_files.as_deref())?;

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

        let mgr = Self {
            inner: Arc::new(TorrentState {
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
                    downloaded_and_checked: Default::default(),
                    fetched_bytes: Default::default(),
                    uploaded: Default::default(),
                },
                needed: initial_check_results.needed_bytes,
                lengths,
            }),
        };

        spawn("tracker monitor", mgr.clone().task_tracker_monitor());
        spawn("stats printer", mgr.clone().stats_printer());
        Ok(mgr.into_handle())
    }

    async fn stats_printer(self) -> anyhow::Result<()> {
        loop {
            let live_peers = self.inner.locked.read().peers.stats();
            let have = self.inner.stats.have.load(Ordering::Relaxed);
            let fetched = self.inner.stats.fetched_bytes.load(Ordering::Relaxed);
            let needed = self.inner.needed;
            let downloaded = self
                .inner
                .stats
                .downloaded_and_checked
                .load(Ordering::Relaxed);
            let remaining = needed - downloaded;
            let uploaded = self.inner.stats.uploaded.load(Ordering::Relaxed);
            let downloaded_pct = if downloaded == needed {
                100f64
            } else {
                (downloaded as f64 / needed as f64) * 100f64
            };
            info!(
                "Stats: downloaded {:.2}% ({}), peers {:?}, fetched {}, remaining {} out of {}, uploaded {}, total have {}",
                downloaded_pct,
                SF::new(downloaded),
                live_peers,
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
        for tracker in self.inner.torrent.iter_announce() {
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
        let bytes = response.bytes().await?;
        let response = crate::serde_bencode::from_bytes::<CompactTrackerResponse>(&bytes)?;

        for peer in response.peers.iter_sockaddrs() {
            self.add_peer(peer);
        }
        Ok(response.interval)
    }

    async fn single_tracker_monitor(self, mut tracker_url: Url) -> anyhow::Result<()> {
        let mut event = Some(TrackerRequestEvent::Started);
        loop {
            let request = TrackerRequest {
                info_hash: self.inner.torrent.info_hash,
                peer_id: self.inner.peer_id,
                port: 6778,
                uploaded: self.inner.get_uploaded(),
                downloaded: self.inner.get_downloaded(),
                left: self.inner.get_left_to_download(),
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
                    let duration = Duration::from_secs(interval);
                    debug!(
                        "sleeping for {:?} after calling tracker {}",
                        duration,
                        tracker_url.host().unwrap()
                    );
                    tokio::time::sleep(duration).await;
                }
                Err(e) => {
                    error!("error calling the tracker {}: {:#}", tracker_url, e);
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
            };
        }
    }

    fn add_peer(&self, addr: SocketAddr) {
        let (out_tx, out_rx) = tokio::sync::mpsc::channel::<WriterRequest>(1);
        let handle = match self
            .inner
            .locked
            .write()
            .peers
            .add_if_not_seen(addr, out_tx)
        {
            Some(handle) => handle,
            None => return,
        };

        let peer_connection = PeerConnection::new(self.inner.clone());
        spawn(format!("manage_peer({})", handle), async move {
            if let Err(e) = peer_connection.manage_peer(addr, handle, out_rx).await {
                error!("error managing peer {}: {:#}", handle, e)
            };
            peer_connection.into_state().drop_peer(handle);
            Ok::<_, anyhow::Error>(())
        });
    }
}
