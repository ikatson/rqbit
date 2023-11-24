use std::{borrow::Cow, io::Read, net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{bail, Context};
use buffers::ByteString;
use dht::{Dht, Id20, PersistentDht, PersistentDhtConfig};
use librqbit_core::{
    magnet::Magnet,
    peer_id::generate_peer_id,
    torrent_metainfo::{torrent_from_bytes, TorrentMetaV1Info, TorrentMetaV1Owned},
};
use parking_lot::RwLock;
use reqwest::Url;
use tokio_stream::StreamExt;
use tracing::{debug, info, span, warn, Level};

use crate::{
    dht_utils::{read_metainfo_from_peer_receiver, ReadMetainfoResult},
    peer_connection::PeerConnectionOptions,
    spawn_utils::{spawn, BlockingSpawner},
    torrent_state::{ManagedTorrentBuilder, ManagedTorrentHandle},
};

pub const SUPPORTED_SCHEMES: [&str; 3] = ["http:", "https:", "magnet:"];

#[derive(Default)]
pub struct SessionLocked {
    torrents: Vec<ManagedTorrentHandle>,
}

enum SessionLockedAddTorrentResult {
    AlreadyManaged(ManagedTorrentHandle),
    Added(usize),
}

impl SessionLocked {
    fn add_torrent(&mut self, torrent: ManagedTorrentHandle) -> SessionLockedAddTorrentResult {
        if let Some(handle) = self
            .torrents
            .iter()
            .find(|t| t.info_hash() == torrent.info_hash())
        {
            return SessionLockedAddTorrentResult::AlreadyManaged(handle.clone());
        }
        let idx = self.torrents.len();
        self.torrents.push(torrent);
        SessionLockedAddTorrentResult::Added(idx)
    }
}

pub struct Session {
    peer_id: Id20,
    dht: Option<Dht>,
    peer_opts: PeerConnectionOptions,
    spawner: BlockingSpawner,
    locked: RwLock<SessionLocked>,
    output_folder: PathBuf,
}

async fn torrent_from_url(url: &str) -> anyhow::Result<TorrentMetaV1Owned> {
    let response = reqwest::get(url)
        .await
        .with_context(|| format!("error downloading torrent metadata from {url}"))?;
    if !response.status().is_success() {
        anyhow::bail!("GET {} returned {}", url, response.status())
    }
    let b = response
        .bytes()
        .await
        .with_context(|| format!("error reading repsonse body from {url}"))?;
    torrent_from_bytes(&b).context("error decoding torrent")
}

fn compute_only_files<ByteBuf: AsRef<[u8]>>(
    torrent: &TorrentMetaV1Info<ByteBuf>,
    filename_re: &str,
) -> anyhow::Result<Vec<usize>> {
    let filename_re = regex::Regex::new(filename_re).context("filename regex is incorrect")?;
    let mut only_files = Vec::new();
    for (idx, (filename, _)) in torrent.iter_filenames_and_lengths()?.enumerate() {
        let full_path = filename
            .to_pathbuf()
            .with_context(|| format!("filename of file {idx} is not valid utf8"))?;
        if filename_re.is_match(full_path.to_str().unwrap()) {
            only_files.push(idx);
        }
    }
    if only_files.is_empty() {
        anyhow::bail!("none of the filenames match the given regex")
    }
    Ok(only_files)
}

#[derive(Default, Clone)]
pub struct AddTorrentOptions {
    pub only_files_regex: Option<String>,
    pub only_files: Option<Vec<usize>>,
    pub overwrite: bool,
    pub list_only: bool,
    pub output_folder: Option<String>,
    pub sub_folder: Option<String>,
    pub peer_opts: Option<PeerConnectionOptions>,
    pub force_tracker_interval: Option<Duration>,
}

pub struct ListOnlyResponse {
    pub info_hash: Id20,
    pub info: TorrentMetaV1Info<ByteString>,
    pub only_files: Option<Vec<usize>>,
}

pub enum AddTorrentResponse {
    AlreadyManaged(ManagedTorrentHandle),
    ListOnly(ListOnlyResponse),
    Added(ManagedTorrentHandle),
}

pub fn read_local_file_including_stdin(filename: &str) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    if filename == "-" {
        std::io::stdin()
            .read_to_end(&mut buf)
            .context("error reading stdin")?;
    } else {
        std::fs::File::open(filename)
            .context("error opening")?
            .read_to_end(&mut buf)
            .context("error reading")?;
    }
    Ok(buf)
}

pub enum AddTorrent<'a> {
    Url(Cow<'a, str>),
    TorrentFileBytes(Cow<'a, [u8]>),
}

impl<'a> AddTorrent<'a> {
    // Don't call this from HTTP API.
    pub fn from_cli_argument(path: &'a str) -> anyhow::Result<Self> {
        if SUPPORTED_SCHEMES.iter().any(|s| path.starts_with(s)) {
            return Ok(Self::Url(Cow::Borrowed(path)));
        }
        Self::from_local_filename(path)
    }

    pub fn from_url(url: impl Into<Cow<'a, str>>) -> Self {
        Self::Url(url.into())
    }

    pub fn from_bytes(bytes: impl Into<Cow<'a, [u8]>>) -> Self {
        Self::TorrentFileBytes(bytes.into())
    }

    // Don't call this from HTTP API.
    pub fn from_local_filename(filename: &str) -> anyhow::Result<Self> {
        let file = read_local_file_including_stdin(filename)
            .with_context(|| format!("error reading local file {filename:?}"))?;
        Ok(Self::TorrentFileBytes(Cow::Owned(file)))
    }

    pub fn into_bytes(self) -> Vec<u8> {
        match self {
            Self::Url(s) => s.into_owned().into_bytes(),
            Self::TorrentFileBytes(b) => b.into_owned(),
        }
    }
}

#[derive(Default)]
pub struct SessionOptions {
    pub disable_dht: bool,
    pub disable_dht_persistence: bool,
    pub dht_config: Option<PersistentDhtConfig>,
    pub peer_id: Option<Id20>,
    pub peer_opts: Option<PeerConnectionOptions>,
}

impl Session {
    pub async fn new(output_folder: PathBuf, spawner: BlockingSpawner) -> anyhow::Result<Self> {
        Self::new_with_opts(output_folder, spawner, SessionOptions::default()).await
    }
    pub async fn new_with_opts(
        output_folder: PathBuf,
        spawner: BlockingSpawner,
        opts: SessionOptions,
    ) -> anyhow::Result<Self> {
        let peer_id = opts.peer_id.unwrap_or_else(generate_peer_id);
        let dht = if opts.disable_dht {
            None
        } else {
            let dht = if opts.disable_dht_persistence {
                Dht::new().await
            } else {
                PersistentDht::create(opts.dht_config).await
            }
            .context("error initializing DHT")?;
            Some(dht)
        };
        let peer_opts = opts.peer_opts.unwrap_or_default();

        Ok(Self {
            peer_id,
            dht,
            peer_opts,
            spawner,
            output_folder,
            locked: RwLock::new(SessionLocked::default()),
        })
    }
    pub fn get_dht(&self) -> Option<Dht> {
        self.dht.clone()
    }
    pub fn with_torrents<F>(&self, callback: F)
    where
        F: Fn(&[ManagedTorrentHandle]),
    {
        callback(&self.locked.read().torrents)
    }
    pub async fn add_torrent(
        &self,
        add: impl Into<AddTorrent<'_>>,
        opts: Option<AddTorrentOptions>,
    ) -> anyhow::Result<AddTorrentResponse> {
        // Magnet links are different in that we first need to discover the metadata.
        let opts = opts.unwrap_or_default();

        let (info_hash, info, dht_rx, trackers, initial_peers) = match add.into() {
            AddTorrent::Url(magnet) if magnet.starts_with("magnet:") => {
                let Magnet {
                    info_hash,
                    trackers,
                } = Magnet::parse(&magnet).context("provided path is not a valid magnet URL")?;

                let dht_rx = self
                    .dht
                    .as_ref()
                    .context("magnet links without DHT are not supported")?
                    .get_peers(info_hash)
                    .await?;

                let trackers = trackers
                    .into_iter()
                    .filter_map(|url| match reqwest::Url::parse(&url) {
                        Ok(url) => Some(url),
                        Err(e) => {
                            warn!("error parsing tracker {} as url: {}", url, e);
                            None
                        }
                    })
                    .collect();

                let (info, dht_rx, initial_peers) = match read_metainfo_from_peer_receiver(
                    self.peer_id,
                    info_hash,
                    dht_rx,
                    Some(self.peer_opts),
                )
                .await
                {
                    ReadMetainfoResult::Found { info, rx, seen } => (info, rx, seen),
                    ReadMetainfoResult::ChannelClosed { .. } => {
                        anyhow::bail!("DHT died, no way to discover torrent metainfo")
                    }
                };
                (info_hash, info, Some(dht_rx), trackers, initial_peers)
            }
            other => {
                let torrent = match other {
                    AddTorrent::Url(url)
                        if url.starts_with("http://") || url.starts_with("https://") =>
                    {
                        torrent_from_url(&url).await?
                    }
                    AddTorrent::Url(url) => {
                        bail!(
                            "unsupported URL {:?}. Supporting magnet:, http:, and https",
                            url
                        )
                    }
                    AddTorrent::TorrentFileBytes(bytes) => {
                        torrent_from_bytes(&bytes).context("error decoding torrent")?
                    }
                };

                let dht_rx = match self.dht.as_ref() {
                    Some(dht) => {
                        debug!("reading peers for {:?} from DHT", torrent.info_hash);
                        Some(dht.get_peers(torrent.info_hash).await?)
                    }
                    None => None,
                };
                let trackers = torrent
                    .iter_announce()
                    .filter_map(|tracker| {
                        let url = match std::str::from_utf8(tracker.as_ref()) {
                            Ok(url) => url,
                            Err(_) => {
                                warn!("cannot parse tracker url as utf-8, ignoring");
                                return None;
                            }
                        };
                        match Url::parse(url) {
                            Ok(url) => Some(url),
                            Err(e) => {
                                warn!("cannot parse tracker URL {}: {}", url, e);
                                None
                            }
                        }
                    })
                    .collect::<Vec<_>>();
                (
                    torrent.info_hash,
                    torrent.info,
                    dht_rx,
                    trackers,
                    Default::default(),
                )
            }
        };

        self.main_torrent_info(
            info_hash,
            info,
            dht_rx,
            initial_peers.into_iter().collect(),
            trackers,
            opts,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn main_torrent_info(
        &self,
        info_hash: Id20,
        info: TorrentMetaV1Info<ByteString>,
        dht_peer_rx: Option<impl StreamExt<Item = SocketAddr> + Unpin + Send + Sync + 'static>,
        initial_peers: Vec<SocketAddr>,
        trackers: Vec<reqwest::Url>,
        opts: AddTorrentOptions,
    ) -> anyhow::Result<AddTorrentResponse> {
        debug!("Torrent info: {:#?}", &info);

        let get_only_files =
            |only_files: Option<Vec<usize>>, only_files_regex: Option<String>, list_only: bool| {
                match (only_files, only_files_regex) {
                    (Some(_), Some(_)) => {
                        bail!("only_files and only_files_regex are mutually exclusive");
                    }
                    (Some(only_files), None) => {
                        let total_files = info.iter_file_lengths()?.count();
                        for id in only_files.iter().copied() {
                            if id >= total_files {
                                anyhow::bail!("file id {} is out of range", id);
                            }
                        }
                        Ok(Some(only_files))
                    }
                    (None, Some(filename_re)) => {
                        let only_files = compute_only_files(&info, &filename_re)?;
                        for (idx, (filename, _)) in info.iter_filenames_and_lengths()?.enumerate() {
                            if !only_files.contains(&idx) {
                                continue;
                            }
                            if !list_only {
                                info!("Will download {:?}", filename);
                            }
                        }
                        Ok(Some(only_files))
                    }
                    (None, None) => Ok(None),
                }
            };

        let only_files = get_only_files(opts.only_files, opts.only_files_regex, opts.list_only)?;

        if opts.list_only {
            return Ok(AddTorrentResponse::ListOnly(ListOnlyResponse {
                info_hash,
                info,
                only_files,
            }));
        }

        let sub_folder = opts.sub_folder.map(PathBuf::from).unwrap_or_default();
        let output_folder = opts
            .output_folder
            .map(PathBuf::from)
            .unwrap_or_else(|| self.output_folder.clone())
            .join(sub_folder);

        let mut builder = ManagedTorrentBuilder::new(info, info_hash, output_folder.clone());
        builder
            .overwrite(opts.overwrite)
            .spawner(self.spawner)
            .peer_id(self.peer_id)
            .trackers(trackers);

        if let Some(only_files) = only_files {
            builder.only_files(only_files);
        }
        if let Some(interval) = opts.force_tracker_interval {
            builder.force_tracker_interval(interval);
        }

        if let Some(t) = opts.peer_opts.unwrap_or(self.peer_opts).connect_timeout {
            builder.peer_connect_timeout(t);
        }

        if let Some(t) = opts.peer_opts.unwrap_or(self.peer_opts).read_write_timeout {
            builder.peer_read_write_timeout(t);
        }

        let managed_torrent = builder.build();

        match self.locked.write().add_torrent(managed_torrent.clone()) {
            SessionLockedAddTorrentResult::AlreadyManaged(managed) => {
                return Ok(AddTorrentResponse::AlreadyManaged(managed))
            }
            SessionLockedAddTorrentResult::Added(_) => {}
        }

        for peer in initial_peers {
            managed_torrent.add_peer(peer);
        }

        if let Some(mut dht_peer_rx) = dht_peer_rx {
            spawn(span!(Level::INFO, "dht_peer_adder"), {
                let handle = managed_torrent.clone();
                async move {
                    while let Some(peer) = dht_peer_rx.next().await {
                        handle.add_peer(peer);
                    }
                    warn!("dht was closed");
                    Ok(())
                }
            });
        }

        Ok(AddTorrentResponse::Added(managed_torrent))
    }
}
