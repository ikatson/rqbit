use std::{
    any::TypeId,
    borrow::Cow,
    collections::{HashMap, HashSet},
    io::{BufReader, BufWriter, Read},
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use crate::{
    dht_utils::{read_metainfo_from_peer_receiver, ReadMetainfoResult},
    merge_streams::merge_streams,
    peer_connection::PeerConnectionOptions,
    read_buf::ReadBuf,
    spawn_utils::BlockingSpawner,
    storage::{
        filesystem::FilesystemStorageFactory, BoxStorageFactory, StorageFactoryExt, TorrentStorage,
    },
    stream_connect::{SocksProxyConfig, StreamConnector},
    torrent_state::{
        ManagedTorrentBuilder, ManagedTorrentHandle, ManagedTorrentState, TorrentStateLive,
    },
    type_aliases::{DiskWorkQueueSender, PeerStream},
    ManagedTorrentInfo,
};
use anyhow::{bail, Context};
use bencode::{bencode_serialize_to_writer, BencodeDeserializer};
use buffers::{ByteBuf, ByteBufOwned, ByteBufT};
use bytes::Bytes;
use clone_to_owned::CloneToOwned;
use dht::{Dht, DhtBuilder, DhtConfig, Id20, PersistentDht, PersistentDhtConfig};
use futures::{
    future::BoxFuture,
    stream::{BoxStream, FuturesUnordered},
    FutureExt, Stream, TryFutureExt,
};
use itertools::Itertools;
use librqbit_core::{
    constants::CHUNK_SIZE,
    directories::get_configuration_directory,
    magnet::Magnet,
    peer_id::generate_peer_id,
    spawn_utils::spawn_with_cancel,
    torrent_metainfo::{
        torrent_from_bytes as bencode_torrent_from_bytes, TorrentMetaV1Borrowed, TorrentMetaV1Info,
        TorrentMetaV1Owned,
    },
};
use parking_lot::RwLock;
use peer_binary_protocol::Handshake;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio::net::{TcpListener, TcpStream};
use tokio_stream::StreamExt;
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::{debug, error, error_span, info, trace, warn, Instrument};
use tracker_comms::TrackerComms;

pub const SUPPORTED_SCHEMES: [&str; 3] = ["http:", "https:", "magnet:"];

pub type TorrentId = usize;

fn torrent_from_bytes(bytes: &[u8]) -> anyhow::Result<TorrentMetaV1Borrowed> {
    debug!(
        "all fields in torrent: {:#?}",
        bencode::dyn_from_bytes::<ByteBuf>(bytes)
    );
    bencode_torrent_from_bytes(bytes)
}

#[derive(Default)]
pub struct SessionDatabase {
    next_id: TorrentId,
    torrents: HashMap<TorrentId, ManagedTorrentHandle>,
}

impl SessionDatabase {
    fn add_torrent(
        &mut self,
        torrent: ManagedTorrentHandle,
        preferred_id: Option<TorrentId>,
    ) -> TorrentId {
        match preferred_id {
            Some(id) if self.torrents.contains_key(&id) => {
                warn!("id {id} already present in DB, ignoring \"preferred_id\" parameter");
            }
            Some(id) => {
                self.torrents.insert(id, torrent);
                self.next_id = id.max(self.next_id).wrapping_add(1);
                return id;
            }
            _ => {}
        }
        let idx = self.next_id;
        self.torrents.insert(idx, torrent);
        self.next_id += 1;
        idx
    }

    fn serialize(&self) -> SerializedSessionDatabase {
        SerializedSessionDatabase {
            torrents: self
                .torrents
                .iter()
                // We don't support serializing / deserializing of other storage types.
                .filter(|(_, torrent)| {
                    torrent
                        .storage_factory
                        .is_type_id(TypeId::of::<FilesystemStorageFactory>())
                })
                .map(|(id, torrent)| {
                    (
                        *id,
                        SerializedTorrent {
                            trackers: torrent
                                .info()
                                .trackers
                                .iter()
                                .map(|u| u.to_string())
                                .collect(),
                            info_hash: torrent.info_hash().as_string(),
                            // TODO: this could take up too much space / time / resources to write on interval.
                            // Store this outside the JSON file
                            //
                            // torrent_bytes: torrent.info.torrent_bytes.clone(),
                            torrent_bytes: Bytes::new(),
                            info: torrent.info().info.clone(),
                            only_files: torrent.only_files().clone(),
                            is_paused: torrent
                                .with_state(|s| matches!(s, ManagedTorrentState::Paused(_))),
                            output_folder: torrent.info().options.output_folder.clone(),
                        },
                    )
                })
                .collect(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct SerializedTorrent {
    info_hash: String,
    #[serde(
        serialize_with = "serialize_torrent",
        deserialize_with = "deserialize_torrent"
    )]
    info: TorrentMetaV1Info<ByteBufOwned>,
    #[serde(
        serialize_with = "serialize_torrent_bytes",
        deserialize_with = "deserialize_torrent_bytes",
        default
    )]
    torrent_bytes: Bytes,
    trackers: HashSet<String>,
    output_folder: PathBuf,
    only_files: Option<Vec<usize>>,
    is_paused: bool,
}

fn serialize_torrent<S>(
    t: &TorrentMetaV1Info<ByteBufOwned>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    use base64::{engine::general_purpose, Engine as _};
    use serde::ser::Error;
    let mut writer = Vec::new();
    bencode_serialize_to_writer(t, &mut writer).map_err(S::Error::custom)?;
    let s = general_purpose::STANDARD_NO_PAD.encode(&writer);
    s.serialize(serializer)
}

fn deserialize_torrent<'de, D>(deserializer: D) -> Result<TorrentMetaV1Info<ByteBufOwned>, D::Error>
where
    D: Deserializer<'de>,
{
    use base64::{engine::general_purpose, Engine as _};
    use serde::de::Error;
    let s = String::deserialize(deserializer)?;
    let b = general_purpose::STANDARD_NO_PAD
        .decode(s)
        .map_err(D::Error::custom)?;
    TorrentMetaV1Info::<ByteBufOwned>::deserialize(&mut BencodeDeserializer::new_from_buf(&b))
        .map_err(D::Error::custom)
}

fn serialize_torrent_bytes<S>(t: &Bytes, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    use base64::{engine::general_purpose, Engine as _};
    let s = general_purpose::STANDARD_NO_PAD.encode(t);
    s.serialize(serializer)
}

fn deserialize_torrent_bytes<'de, D>(deserializer: D) -> Result<Bytes, D::Error>
where
    D: Deserializer<'de>,
{
    use base64::{engine::general_purpose, Engine as _};
    use serde::de::Error;
    let s = String::deserialize(deserializer)?;
    let b = general_purpose::STANDARD_NO_PAD
        .decode(s)
        .map_err(D::Error::custom)?;
    Ok(b.into())
}

#[derive(Serialize, Deserialize)]
struct SerializedSessionDatabase {
    torrents: HashMap<usize, SerializedTorrent>,
}

pub struct Session {
    peer_id: Id20,
    dht: Option<Dht>,
    persistence_filename: PathBuf,
    peer_opts: PeerConnectionOptions,
    spawner: BlockingSpawner,
    db: RwLock<SessionDatabase>,
    output_folder: PathBuf,

    tcp_listen_port: Option<u16>,

    cancellation_token: CancellationToken,

    disk_write_tx: Option<DiskWorkQueueSender>,

    default_storage_factory: Option<BoxStorageFactory>,

    reqwest_client: reqwest::Client,
    connector: Arc<StreamConnector>,

    // This is stored for all tasks to stop when session is dropped.
    _cancellation_token_drop_guard: DropGuard,
}

async fn torrent_from_url(
    reqwest_client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<(TorrentMetaV1Owned, ByteBufOwned)> {
    let response = reqwest_client
        .get(url)
        .send()
        .await
        .context("error downloading torrent metadata")?;
    if !response.status().is_success() {
        bail!("GET {} returned {}", url, response.status())
    }
    let b = response
        .bytes()
        .await
        .with_context(|| format!("error reading response body from {url}"))?;
    Ok((
        torrent_from_bytes(&b)
            .context("error decoding torrent")?
            .clone_to_owned(Some(&b)),
        b.into(),
    ))
}

fn compute_only_files_regex<ByteBuf: AsRef<[u8]>>(
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
        bail!("none of the filenames match the given regex")
    }
    Ok(only_files)
}

fn compute_only_files(
    info: &TorrentMetaV1Info<ByteBufOwned>,
    only_files: Option<Vec<usize>>,
    only_files_regex: Option<String>,
    list_only: bool,
) -> anyhow::Result<Option<Vec<usize>>> {
    match (only_files, only_files_regex) {
        (Some(_), Some(_)) => {
            bail!("only_files and only_files_regex are mutually exclusive");
        }
        (Some(only_files), None) => {
            let total_files = info.iter_file_lengths()?.count();
            for id in only_files.iter().copied() {
                if id >= total_files {
                    bail!("file id {} is out of range", id);
                }
            }
            Ok(Some(only_files))
        }
        (None, Some(filename_re)) => {
            let only_files = compute_only_files_regex(info, &filename_re)?;
            for (idx, (filename, _)) in info.iter_filenames_and_lengths()?.enumerate() {
                if !only_files.contains(&idx) {
                    continue;
                }
                if !list_only {
                    info!(?filename, "will download");
                }
            }
            Ok(Some(only_files))
        }
        (None, None) => Ok(None),
    }
}

fn merge_two_optional_streams<T>(
    s1: Option<impl Stream<Item = T> + Unpin + Send + 'static>,
    s2: Option<impl Stream<Item = T> + Unpin + Send + 'static>,
) -> Option<BoxStream<'static, T>> {
    match (s1, s2) {
        (Some(s1), None) => Some(Box::pin(s1)),
        (None, Some(s2)) => Some(Box::pin(s2)),
        (Some(s1), Some(s2)) => Some(Box::pin(merge_streams(s1, s2))),
        (None, None) => None,
    }
}

/// Options for adding new torrents to the session.
//
// Serialize/deserialize is for Tauri.
#[derive(Default, Serialize, Deserialize)]
pub struct AddTorrentOptions {
    /// Start in paused state.
    #[serde(default)]
    pub paused: bool,
    /// A regex to only download files matching it.
    pub only_files_regex: Option<String>,
    /// An explicit list of file IDs to download.
    /// To see the file indices, run with "list_only".
    pub only_files: Option<Vec<usize>>,
    /// Allow writing on top of existing files, including when resuming a torrent.
    /// You probably want to set it, however for safety it's not default.
    #[serde(default)]
    pub overwrite: bool,
    /// Only list the files in the torrent without starting it.
    #[serde(default)]
    pub list_only: bool,
    /// The output folder for the torrent. If not set, the session's default one will be used.
    pub output_folder: Option<String>,
    /// Sub-folder within session's default output folder. Will error if "output_folder" if also set.
    /// By default, multi-torrent files are downloaded to a sub-folder.
    pub sub_folder: Option<String>,
    /// Peer connection options, timeouts etc. If not set, session's defaults will be used.
    pub peer_opts: Option<PeerConnectionOptions>,

    /// Force a refresh interval for polling trackers.
    pub force_tracker_interval: Option<Duration>,

    #[serde(default)]
    pub disable_trackers: bool,

    /// Initial peers to start of with.
    pub initial_peers: Option<Vec<SocketAddr>>,

    /// This is used to restore the session from serialized state.
    pub preferred_id: Option<usize>,

    #[serde(skip)]
    pub storage_factory: Option<BoxStorageFactory>,

    // If true, will write to disk in separate threads. The downside is additional allocations.
    // May be useful if the disk is slow.
    pub defer_writes: Option<bool>,
}

pub struct ListOnlyResponse {
    pub info_hash: Id20,
    pub info: TorrentMetaV1Info<ByteBufOwned>,
    pub only_files: Option<Vec<usize>>,
    pub output_folder: PathBuf,
    pub seen_peers: Vec<SocketAddr>,
    pub torrent_bytes: Bytes,
}

#[allow(clippy::large_enum_variant)]
pub enum AddTorrentResponse {
    AlreadyManaged(TorrentId, ManagedTorrentHandle),
    ListOnly(ListOnlyResponse),
    Added(TorrentId, ManagedTorrentHandle),
}

impl AddTorrentResponse {
    pub fn into_handle(self) -> Option<ManagedTorrentHandle> {
        match self {
            Self::AlreadyManaged(_, handle) => Some(handle),
            Self::ListOnly(_) => None,
            Self::Added(_, handle) => Some(handle),
        }
    }
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
    TorrentInfo(Box<TorrentMetaV1Owned>, Bytes),
}

impl<'a> AddTorrent<'a> {
    // Don't call this from HTTP API.
    #[inline(never)]
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
    #[inline(never)]
    pub fn from_local_filename(filename: &str) -> anyhow::Result<Self> {
        let file = read_local_file_including_stdin(filename)
            .with_context(|| format!("error reading local file {filename:?}"))?;
        Ok(Self::TorrentFileBytes(Cow::Owned(file)))
    }

    pub fn into_bytes(self) -> Vec<u8> {
        match self {
            Self::Url(s) => s.into_owned().into_bytes(),
            Self::TorrentFileBytes(b) => b.into_owned(),
            Self::TorrentInfo(..) => unimplemented!(),
        }
    }
}

#[derive(Default)]
pub struct SessionOptions {
    /// Turn on to disable DHT.
    pub disable_dht: bool,
    /// Turn on to disable DHT persistence. By default it will re-use stored DHT
    /// configuration, including the port it listens on.
    pub disable_dht_persistence: bool,
    /// Pass in to configure DHT persistence filename. This can be used to run multiple
    /// librqbit instances at a time.
    pub dht_config: Option<PersistentDhtConfig>,

    /// Turn on to dump session contents into a file periodically, so that on next start
    /// all remembered torrents will continue where they left off.
    pub persistence: bool,
    /// The filename for persistence. By default uses an OS-specific folder.
    pub persistence_filename: Option<PathBuf>,

    /// The peer ID to use. If not specified, a random one will be generated.
    pub peer_id: Option<Id20>,
    /// Configure default peer connection options. Can be overriden per torrent.
    pub peer_opts: Option<PeerConnectionOptions>,

    pub listen_port_range: Option<std::ops::Range<u16>>,
    pub enable_upnp_port_forwarding: bool,

    // If you set this to something, all writes to disk will happen in background and be
    // buffered in memory up to approximately the given number of megabytes.
    pub defer_writes_up_to: Option<usize>,

    pub default_storage_factory: Option<BoxStorageFactory>,

    // socks5://[username:password@]host:port
    pub socks_proxy_url: Option<String>,
}

async fn create_tcp_listener(
    port_range: std::ops::Range<u16>,
) -> anyhow::Result<(TcpListener, u16)> {
    for port in port_range.clone() {
        match TcpListener::bind(("0.0.0.0", port)).await {
            Ok(l) => return Ok((l, port)),
            Err(e) => {
                debug!("error listening on port {port}: {e:#}")
            }
        }
    }
    bail!("no free TCP ports in range {port_range:?}");
}

fn torrent_file_from_info_bytes(info_bytes: &[u8], trackers: &[String]) -> anyhow::Result<Bytes> {
    #[derive(Serialize)]
    struct Tmp<'a> {
        announce: &'a str,
        #[serde(rename = "announce-list")]
        announce_list: &'a [&'a [String]],
        info: bencode::raw_value::RawValue<&'a [u8]>,
    }

    let mut w = Vec::new();
    let v = Tmp {
        info: bencode::raw_value::RawValue(info_bytes),
        announce: trackers.first().map(|s| s.as_str()).unwrap_or(""),
        announce_list: &[trackers],
    };
    bencode_serialize_to_writer(&v, &mut w)?;
    Ok(w.into())
}

pub(crate) struct CheckedIncomingConnection {
    pub addr: SocketAddr,
    pub stream: tokio::net::TcpStream,
    pub read_buf: ReadBuf,
    pub handshake: Handshake<ByteBufOwned>,
}

struct InternalAddResult {
    info_hash: Id20,
    info: TorrentMetaV1Info<ByteBufOwned>,
    torrent_bytes: Bytes,
    trackers: Vec<String>,
    peer_rx: Option<PeerStream>,
    initial_peers: Vec<SocketAddr>,
}

impl Session {
    /// Create a new session with default options.
    /// The passed in folder will be used as a default unless overriden per torrent.
    /// It will run a DHT server/client, a TCP listener and .
    #[inline(never)]
    pub fn new(default_output_folder: PathBuf) -> BoxFuture<'static, anyhow::Result<Arc<Self>>> {
        Self::new_with_opts(default_output_folder, SessionOptions::default())
    }

    pub fn default_persistence_filename() -> anyhow::Result<PathBuf> {
        let dir = get_configuration_directory("session")?;
        Ok(dir.data_dir().join("session.json"))
    }

    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancellation_token
    }

    /// Create a new session with options.
    #[inline(never)]
    pub fn new_with_opts(
        default_output_folder: PathBuf,
        mut opts: SessionOptions,
    ) -> BoxFuture<'static, anyhow::Result<Arc<Self>>> {
        async move {
            let peer_id = opts.peer_id.unwrap_or_else(generate_peer_id);
            let token = CancellationToken::new();

            let (tcp_listener, tcp_listen_port) = if let Some(port_range) = opts.listen_port_range {
                let (l, p) = create_tcp_listener(port_range)
                    .await
                    .context("error listening on TCP")?;
                info!("Listening on 0.0.0.0:{p} for incoming peer connections");
                (Some(l), Some(p))
            } else {
                (None, None)
            };

            let dht = if opts.disable_dht {
                None
            } else {
                let dht = if opts.disable_dht_persistence {
                    DhtBuilder::with_config(DhtConfig {
                        cancellation_token: Some(token.child_token()),
                        ..Default::default()
                    })
                    .await
                    .context("error initializing DHT")?
                } else {
                    let pdht_config = opts.dht_config.take().unwrap_or_default();
                    PersistentDht::create(Some(pdht_config), Some(token.clone()))
                        .await
                        .context("error initializing persistent DHT")?
                };

                Some(dht)
            };
            let peer_opts = opts.peer_opts.unwrap_or_default();
            let persistence_filename = match opts.persistence_filename {
                Some(filename) => filename,
                None if !opts.persistence => PathBuf::new(),
                None => Self::default_persistence_filename()?,
            };
            let spawner = BlockingSpawner::default();

            let (disk_write_tx, disk_write_rx) = opts
                .defer_writes_up_to
                .map(|mb| {
                    const DISK_WRITE_APPROX_WORK_ITEM_SIZE: usize = CHUNK_SIZE as usize + 300;
                    let count = mb * 1024 * 1024 / DISK_WRITE_APPROX_WORK_ITEM_SIZE;
                    let (tx, rx) = tokio::sync::mpsc::channel(count);
                    (Some(tx), Some(rx))
                })
                .unwrap_or_default();

            let proxy_config = match opts.socks_proxy_url.as_ref() {
                Some(pu) => Some(
                    SocksProxyConfig::parse(pu)
                        .with_context(|| format!("error parsing proxy url {}", pu))?,
                ),
                None => None,
            };

            let reqwest_client = {
                let builder = if let Some(proxy_url) = opts.socks_proxy_url.as_ref() {
                    let proxy = reqwest::Proxy::all(proxy_url)
                        .context("error creating socks5 proxy for HTTP")?;
                    reqwest::Client::builder().proxy(proxy)
                } else {
                    reqwest::Client::builder()
                };

                builder.build().context("error building HTTP(S) client")?
            };

            let stream_connector = Arc::new(StreamConnector::from(proxy_config));

            let session = Arc::new(Self {
                persistence_filename,
                peer_id,
                dht,
                peer_opts,
                spawner,
                output_folder: default_output_folder,
                db: RwLock::new(Default::default()),
                _cancellation_token_drop_guard: token.clone().drop_guard(),
                cancellation_token: token,
                tcp_listen_port,
                disk_write_tx,
                default_storage_factory: opts.default_storage_factory,
                reqwest_client,
                connector: stream_connector,
            });

            if let Some(mut disk_write_rx) = disk_write_rx {
                session.spawn(error_span!("disk_writer"), async move {
                    while let Some(work) = disk_write_rx.recv().await {
                        trace!(disk_write_rx_queue_len = disk_write_rx.len());
                        spawner.spawn_block_in_place(work);
                    }
                    Ok(())
                });
            }

            if let Some(tcp_listener) = tcp_listener {
                session.spawn(
                    error_span!("tcp_listen", port = tcp_listen_port),
                    session.clone().task_tcp_listener(tcp_listener),
                );
            }

            if let Some(listen_port) = tcp_listen_port {
                if opts.enable_upnp_port_forwarding {
                    session.spawn(
                        error_span!("upnp_forward", port = listen_port),
                        session.clone().task_upnp_port_forwarder(listen_port),
                    );
                }
            }

            if opts.persistence {
                info!(
                    "will use {:?} for session persistence",
                    session.persistence_filename
                );
                if let Some(parent) = session.persistence_filename.parent() {
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!("couldn't create directory {:?} for session storage", parent)
                    })?;
                }
                let persistence_task = session.clone().task_persistence();
                session.spawn(error_span!("session_persistence"), persistence_task);
            }

            Ok(session)
        }
        .boxed()
    }

    async fn task_persistence(self: Arc<Self>) -> anyhow::Result<()> {
        // Populate initial from the state filename
        if let Err(e) = self.populate_from_stored().await {
            error!("could not populate session from stored file: {:?}", e);
        }

        let session = Arc::downgrade(&self);
        drop(self);

        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let session = match session.upgrade() {
                Some(s) => s,
                None => break,
            };
            if let Err(e) = session.dump_to_disk() {
                error!("error dumping session to disk: {:?}", e);
            }
        }

        Ok(())
    }

    async fn check_incoming_connection(
        &self,
        addr: SocketAddr,
        mut stream: TcpStream,
    ) -> anyhow::Result<(Arc<TorrentStateLive>, CheckedIncomingConnection)> {
        let rwtimeout = self
            .peer_opts
            .read_write_timeout
            .unwrap_or_else(|| Duration::from_secs(10));

        let mut read_buf = ReadBuf::new();
        let h = read_buf
            .read_handshake(&mut stream, rwtimeout)
            .await
            .context("error reading handshake")?;
        trace!("received handshake from {addr}: {:?}", h);

        if h.peer_id == self.peer_id.0 {
            bail!("seems like we are connecting to ourselves, ignoring");
        }

        for (id, torrent) in self.db.read().torrents.iter() {
            if torrent.info_hash().0 != h.info_hash {
                continue;
            }

            let live = match torrent.live() {
                Some(live) => live,
                None => {
                    bail!("torrent {id} is not live, ignoring connection");
                }
            };

            let handshake = h.clone_to_owned(None);

            return Ok((
                live,
                CheckedIncomingConnection {
                    addr,
                    stream,
                    handshake,
                    read_buf,
                },
            ));
        }

        bail!(
            "didn't find a matching torrent for {:?}",
            Id20::new(h.info_hash)
        )
    }

    async fn task_tcp_listener(self: Arc<Self>, l: TcpListener) -> anyhow::Result<()> {
        let mut futs = FuturesUnordered::new();

        loop {
            tokio::select! {
                r = l.accept() => {
                    match r {
                        Ok((stream, addr)) => {
                            trace!("accepted connection from {addr}");
                            futs.push(
                                self.check_incoming_connection(addr, stream)
                                    .map_err(|e| {
                                        debug!("error checking incoming connection: {e:#}");
                                        e
                                    })
                                    .instrument(error_span!("incoming", addr=%addr))
                            );
                        }
                        Err(e) => {
                            error!("error accepting: {e:#}");
                            continue;
                        }
                    }
                },
                Some(Ok((live, checked))) = futs.next(), if !futs.is_empty() => {
                    if let Err(e) = live.add_incoming_peer(checked) {
                        warn!("error handing over incoming connection: {e:#}");
                    }
                },
            }
        }
    }

    async fn task_upnp_port_forwarder(self: Arc<Self>, port: u16) -> anyhow::Result<()> {
        let pf = librqbit_upnp::UpnpPortForwarder::new(vec![port], None)?;
        pf.run_forever().await
    }

    pub fn get_dht(&self) -> Option<&Dht> {
        self.dht.as_ref()
    }

    fn merge_peer_opts(&self, other: Option<PeerConnectionOptions>) -> PeerConnectionOptions {
        let other = match other {
            Some(o) => o,
            None => self.peer_opts,
        };
        PeerConnectionOptions {
            connect_timeout: other.connect_timeout.or(self.peer_opts.connect_timeout),
            read_write_timeout: other
                .read_write_timeout
                .or(self.peer_opts.read_write_timeout),
            keep_alive_interval: other
                .keep_alive_interval
                .or(self.peer_opts.keep_alive_interval),
        }
    }

    /// Spawn a task in the context of the session.
    pub fn spawn(
        &self,
        span: tracing::Span,
        fut: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    ) {
        spawn_with_cancel(span, self.cancellation_token.clone(), fut);
    }

    /// Stop the session and all managed tasks.
    pub async fn stop(&self) {
        let torrents = self
            .db
            .read()
            .torrents
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for torrent in torrents {
            if let Err(e) = torrent.pause() {
                debug!("error pausing torrent: {e:#}");
            }
        }
        self.cancellation_token.cancel();
        // this sucks, but hopefully will be enough
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    async fn populate_from_stored(self: &Arc<Self>) -> anyhow::Result<()> {
        let mut rdr = match std::fs::File::open(&self.persistence_filename) {
            Ok(f) => BufReader::new(f),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(e).context(format!(
                    "error opening session file {:?}",
                    self.persistence_filename
                ))
            }
        };
        let db: SerializedSessionDatabase =
            serde_json::from_reader(&mut rdr).context("error deserializing session database")?;
        let mut futures = Vec::new();
        for (id, storrent) in db.torrents.into_iter() {
            let trackers: Vec<ByteBufOwned> = storrent
                .trackers
                .into_iter()
                .map(|t| ByteBufOwned::from(t.into_bytes()))
                .collect();

            let torrent_bytes = storrent.torrent_bytes;

            let info = if !torrent_bytes.is_empty() {
                torrent_from_bytes(&torrent_bytes)
                    .map(|t| t.clone_to_owned(Some(&torrent_bytes)))
                    .ok()
            } else {
                None
            };
            let info = match info {
                Some(info) => info,
                None => {
                    let info_hash = Id20::from_str(&storrent.info_hash)?;
                    debug!(?info_hash, "torrent added before 6.1.0, need to readd");
                    TorrentMetaV1Owned {
                        announce: trackers.first().cloned(),
                        announce_list: vec![trackers],
                        info: storrent.info,
                        comment: None,
                        created_by: None,
                        encoding: None,
                        publisher: None,
                        publisher_url: None,
                        creation_date: None,
                        info_hash,
                    }
                }
            };

            futures.push({
                let session = self.clone();
                async move {
                    session
                        .add_torrent(
                            AddTorrent::TorrentInfo(Box::new(info), torrent_bytes),
                            Some(AddTorrentOptions {
                                paused: storrent.is_paused,
                                output_folder: Some(
                                    storrent
                                        .output_folder
                                        .to_str()
                                        .context("broken path")?
                                        .to_owned(),
                                ),
                                only_files: storrent.only_files,
                                overwrite: true,
                                preferred_id: Some(id),
                                ..Default::default()
                            }),
                        )
                        .await
                        .map_err(|e| {
                            error!("error adding torrent from stored session: {:?}", e);
                            e
                        })
                }
            });
        }
        futures::future::join_all(futures).await;
        Ok(())
    }

    fn dump_to_disk(&self) -> anyhow::Result<()> {
        let tmp_filename = format!("{}.tmp", self.persistence_filename.to_str().unwrap());
        let mut tmp = BufWriter::new(
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&tmp_filename)
                .with_context(|| format!("error opening {:?}", tmp_filename))?,
        );
        let serialized = self.db.read().serialize();
        serde_json::to_writer(&mut tmp, &serialized).context("error serializing")?;
        drop(tmp);

        std::fs::rename(&tmp_filename, &self.persistence_filename)
            .context("error renaming persistence file")?;
        trace!(filename=?self.persistence_filename, "wrote persistence");
        Ok(())
    }

    /// Run a callback given the currently managed torrents.
    pub fn with_torrents<R>(
        &self,
        callback: impl Fn(&mut dyn Iterator<Item = (TorrentId, &ManagedTorrentHandle)>) -> R,
    ) -> R {
        callback(&mut self.db.read().torrents.iter().map(|(id, t)| (*id, t)))
    }

    /// Add a torrent to the session.
    #[inline(never)]
    pub fn add_torrent<'a>(
        self: &'a Arc<Self>,
        add: AddTorrent<'a>,
        opts: Option<AddTorrentOptions>,
    ) -> BoxFuture<'a, anyhow::Result<AddTorrentResponse>> {
        async move {
            // Magnet links are different in that we first need to discover the metadata.
            let opts = opts.unwrap_or_default();

            let paused = opts.list_only || opts.paused;

            let announce_port = if paused { None } else { self.tcp_listen_port };

            // The main difference between magnet link and torrent file, is that we need to resolve the magnet link
            // into a torrent file by connecting to peers that support extended handshakes.
            // So we must discover at least one peer and connect to it to be able to proceed further.

            let add_res = match add {
                AddTorrent::Url(magnet) if magnet.starts_with("magnet:") => {
                    let magnet = Magnet::parse(&magnet)
                        .context("provided path is not a valid magnet URL")?;
                    let info_hash = magnet
                        .as_id20()
                        .context("magnet link didn't contain a BTv1 infohash")?;

                    let peer_rx = self.make_peer_rx(
                        info_hash,
                        if opts.disable_trackers {
                            Default::default()
                        } else {
                            magnet.trackers.clone()
                        },
                        announce_port,
                        opts.force_tracker_interval,
                    )?;
                    let peer_rx = match peer_rx {
                        Some(peer_rx) => peer_rx,
                        None => bail!("can't find peers: DHT disabled and no trackers in magnet"),
                    };

                    debug!(?info_hash, "querying DHT");
                    match read_metainfo_from_peer_receiver(
                        self.peer_id,
                        info_hash,
                        opts.initial_peers.clone().unwrap_or_default(),
                        peer_rx,
                        Some(self.merge_peer_opts(opts.peer_opts)),
                        self.connector.clone(),
                    )
                    .await
                    {
                        ReadMetainfoResult::Found {
                            info,
                            info_bytes,
                            rx,
                            seen,
                        } => {
                            debug!(?info, "received result from DHT");
                            let trackers = magnet.trackers.into_iter().unique().collect_vec();
                            InternalAddResult {
                                info_hash,
                                torrent_bytes: torrent_file_from_info_bytes(
                                    &info_bytes,
                                    &trackers,
                                )?,
                                info,
                                trackers,
                                peer_rx: Some(rx),
                                initial_peers: seen.into_iter().collect(),
                            }
                        }
                        ReadMetainfoResult::ChannelClosed { .. } => {
                            bail!("DHT died, no way to discover torrent metainfo")
                        }
                    }
                }
                other => {
                    let (torrent, bytes) = match other {
                        AddTorrent::Url(url)
                            if url.starts_with("http://") || url.starts_with("https://") =>
                        {
                            torrent_from_url(&self.reqwest_client, &url).await?
                        }
                        AddTorrent::Url(url) => {
                            bail!(
                                "unsupported URL {:?}. Supporting magnet:, http:, and https",
                                url
                            )
                        }
                        AddTorrent::TorrentFileBytes(bytes) => {
                            let bytes = match bytes {
                                Cow::Borrowed(b) => ::bytes::Bytes::copy_from_slice(b),
                                Cow::Owned(v) => ::bytes::Bytes::from(v),
                            };
                            (
                                torrent_from_bytes(&bytes)
                                    .map(|t| t.clone_to_owned(Some(&bytes)))
                                    .context("error decoding torrent")?,
                                ByteBufOwned(bytes),
                            )
                        }
                        AddTorrent::TorrentInfo(t, bytes) => (*t, bytes.into()),
                    };

                    let trackers = torrent
                        .iter_announce()
                        .unique()
                        .filter_map(|tracker| match std::str::from_utf8(tracker.as_ref()) {
                            Ok(url) => Some(url.to_owned()),
                            Err(_) => {
                                warn!("cannot parse tracker url as utf-8, ignoring");
                                None
                            }
                        })
                        .collect::<Vec<_>>();

                    let peer_rx = if paused {
                        None
                    } else {
                        self.make_peer_rx(
                            torrent.info_hash,
                            if opts.disable_trackers {
                                Default::default()
                            } else {
                                trackers.clone()
                            },
                            announce_port,
                            opts.force_tracker_interval,
                        )?
                    };

                    InternalAddResult {
                        info_hash: torrent.info_hash,
                        info: torrent.info,
                        torrent_bytes: bytes.0,
                        trackers,
                        peer_rx,
                        initial_peers: opts
                            .initial_peers
                            .clone()
                            .unwrap_or_default()
                            .into_iter()
                            .collect(),
                    }
                }
            };

            self.main_torrent_info(add_res, opts).await
        }
        .instrument(error_span!("add_torrent"))
        .boxed()
    }

    fn get_default_subfolder_for_torrent(
        &self,
        info: &TorrentMetaV1Info<ByteBufOwned>,
    ) -> anyhow::Result<Option<PathBuf>> {
        let files = info
            .iter_filenames_and_lengths()?
            .map(|(f, l)| Ok((f.to_pathbuf()?, l)))
            .collect::<anyhow::Result<Vec<(PathBuf, u64)>>>()?;
        if files.len() < 2 {
            return Ok(None);
        }
        if let Some(name) = &info.name {
            let s =
                std::str::from_utf8(name.as_slice()).context("invalid UTF-8 in torrent name")?;
            return Ok(Some(PathBuf::from(s)));
        };
        // Let the subfolder name be the longest filename
        let longest = files
            .iter()
            .max_by_key(|(_, l)| l)
            .unwrap()
            .0
            .file_stem()
            .context("can't determine longest filename")?;
        Ok::<_, anyhow::Error>(Some(PathBuf::from(longest)))
    }

    async fn main_torrent_info(
        &self,
        add_res: InternalAddResult,
        mut opts: AddTorrentOptions,
    ) -> anyhow::Result<AddTorrentResponse> {
        let InternalAddResult {
            info,
            info_hash,
            trackers,
            peer_rx,
            initial_peers,
            torrent_bytes,
        } = add_res;

        debug!("Torrent info: {:#?}", &info);

        let only_files = compute_only_files(
            &info,
            opts.only_files,
            opts.only_files_regex,
            opts.list_only,
        )?;

        let output_folder = match (opts.output_folder, opts.sub_folder) {
            (None, None) => self.output_folder.join(
                self.get_default_subfolder_for_torrent(&info)?
                    .unwrap_or_default(),
            ),
            (Some(o), None) => PathBuf::from(o),
            (Some(_), Some(_)) => {
                bail!("you can't provide both output_folder and sub_folder")
            }
            (None, Some(s)) => self.output_folder.join(s),
        };

        let storage_factory = opts
            .storage_factory
            .take()
            .or_else(|| self.default_storage_factory.as_ref().map(|f| f.clone_box()))
            .unwrap_or_else(|| FilesystemStorageFactory::default().boxed());

        if opts.list_only {
            return Ok(AddTorrentResponse::ListOnly(ListOnlyResponse {
                info_hash,
                info,
                only_files,
                output_folder,
                seen_peers: initial_peers,
                torrent_bytes,
            }));
        }

        let mut builder = ManagedTorrentBuilder::new(
            info,
            info_hash,
            torrent_bytes,
            output_folder,
            storage_factory,
        );
        builder
            .allow_overwrite(opts.overwrite)
            .spawner(self.spawner)
            .trackers(trackers)
            .connector(self.connector.clone())
            .peer_id(self.peer_id);

        if let Some(d) = self.disk_write_tx.clone() {
            builder.disk_writer(d);
        }

        if let Some(only_files) = only_files {
            builder.only_files(only_files);
        }
        if let Some(interval) = opts.force_tracker_interval {
            builder.force_tracker_interval(interval);
        }

        let peer_opts = self.merge_peer_opts(opts.peer_opts);

        if let Some(t) = peer_opts.connect_timeout {
            builder.peer_connect_timeout(t);
        }

        if let Some(t) = peer_opts.read_write_timeout {
            builder.peer_read_write_timeout(t);
        }

        let (managed_torrent, id) = {
            let mut g = self.db.write();
            if let Some((id, handle)) = g.torrents.iter().find(|(_, t)| t.info_hash() == info_hash)
            {
                return Ok(AddTorrentResponse::AlreadyManaged(*id, handle.clone()));
            }
            let next_id = g.torrents.len();
            let managed_torrent =
                builder.build(error_span!(parent: None, "torrent", id = next_id))?;
            let id = g.add_torrent(managed_torrent.clone(), opts.preferred_id);
            (managed_torrent, id)
        };

        // Merge "initial_peers" and "peer_rx" into one stream.
        let peer_rx = merge_two_optional_streams(
            if !initial_peers.is_empty() {
                Some(futures::stream::iter(initial_peers.into_iter()))
            } else {
                None
            },
            peer_rx,
        );

        {
            let span = managed_torrent.info.span.clone();
            let _ = span.enter();

            managed_torrent
                .start(peer_rx, opts.paused, self.cancellation_token.child_token())
                .context("error starting torrent")?;
        }

        Ok(AddTorrentResponse::Added(id, managed_torrent))
    }

    pub fn get(&self, id: TorrentId) -> Option<ManagedTorrentHandle> {
        self.db.read().torrents.get(&id).cloned()
    }

    pub fn delete(&self, id: TorrentId, delete_files: bool) -> anyhow::Result<()> {
        let removed = self
            .db
            .write()
            .torrents
            .remove(&id)
            .with_context(|| format!("torrent with id {} did not exist", id))?;

        if let Err(e) = removed.pause() {
            debug!("error pausing torrent before deletion: {e:?}")
        }

        let storage = removed
            .with_state_mut(|s| match s.take() {
                ManagedTorrentState::Initializing(p) => p.files.take().ok(),
                ManagedTorrentState::Paused(p) => Some(p.files),
                ManagedTorrentState::Live(l) => l
                    .pause()
                    // inspect_err not available in 1.75
                    .map_err(|e| {
                        warn!("error pausing torrent: {e:?}");
                        e
                    })
                    .ok()
                    .map(|p| p.files),
                _ => None,
            })
            .map(Ok)
            .unwrap_or_else(|| removed.storage_factory.create(removed.info()));

        match (storage, delete_files) {
            (Err(e), true) => return Err(e).context("torrent deleted, but could not delete files"),
            (Ok(storage), true) => {
                debug!("will delete files");
                remove_files_and_dirs(removed.info(), &storage);
                if removed.info().options.output_folder != self.output_folder {
                    if let Err(e) = storage.remove_directory_if_empty(Path::new("")) {
                        warn!(
                            "error removing {:?}: {e:?}",
                            removed.info().options.output_folder
                        )
                    }
                }
            }
            (_, false) => {
                debug!("not deleting files")
            }
        };

        info!(id, "deleted torrent");
        Ok(())
    }

    // Get a peer stream from both DHT and trackers.
    fn make_peer_rx(
        self: &Arc<Self>,
        info_hash: Id20,
        trackers: Vec<String>,
        announce_port: Option<u16>,
        force_tracker_interval: Option<Duration>,
    ) -> anyhow::Result<Option<PeerStream>> {
        let announce_port = announce_port.or(self.tcp_listen_port);
        let dht_rx = self
            .dht
            .as_ref()
            .map(|dht| dht.get_peers(info_hash, announce_port))
            .transpose()?;

        let peer_rx_stats = PeerRxTorrentInfo {
            info_hash,
            session: self.clone(),
        };
        let peer_rx = TrackerComms::start(
            info_hash,
            self.peer_id,
            trackers,
            Box::new(peer_rx_stats),
            force_tracker_interval,
            announce_port,
            self.reqwest_client.clone(),
        );

        Ok(merge_two_optional_streams(dht_rx, peer_rx))
    }

    pub fn unpause(self: &Arc<Self>, handle: &ManagedTorrentHandle) -> anyhow::Result<()> {
        let peer_rx = self.make_peer_rx(
            handle.info_hash(),
            handle.info().trackers.clone().into_iter().collect(),
            self.tcp_listen_port,
            handle.info().options.force_tracker_interval,
        )?;
        handle.start(peer_rx, false, self.cancellation_token.child_token())?;
        Ok(())
    }

    pub fn update_only_files(
        self: &Arc<Self>,
        handle: &ManagedTorrentHandle,
        only_files: &HashSet<usize>,
    ) -> anyhow::Result<()> {
        handle.update_only_files(only_files)?;
        Ok(())
    }

    pub fn tcp_listen_port(&self) -> Option<u16> {
        self.tcp_listen_port
    }
}

fn remove_files_and_dirs(info: &ManagedTorrentInfo, files: &dyn TorrentStorage) {
    let mut all_dirs = HashSet::new();
    for (id, fi) in info.file_infos.iter().enumerate() {
        let mut fname = &*fi.relative_filename;
        if let Err(e) = files.remove_file(id, fname) {
            warn!(?fi.relative_filename, error=?e, "could not delete file");
        } else {
            debug!(?fi.relative_filename, "deleted the file")
        }
        while let Some(parent) = fname.parent() {
            if parent != Path::new("") {
                all_dirs.insert(parent);
            }
            fname = parent;
        }
    }

    let all_dirs = {
        let mut v = all_dirs.into_iter().collect::<Vec<_>>();
        v.sort_unstable_by_key(|p| std::cmp::Reverse(p.as_os_str().len()));
        v
    };
    for dir in all_dirs {
        if let Err(e) = files.remove_directory_if_empty(dir) {
            warn!("error removing {dir:?}: {e:?}");
        } else {
            debug!("removed {dir:?}")
        }
    }
}

// Ad adapter for converting stats into the format that tracker_comms accepts.
struct PeerRxTorrentInfo {
    info_hash: Id20,
    session: Arc<Session>,
}

impl tracker_comms::TorrentStatsProvider for PeerRxTorrentInfo {
    fn get(&self) -> tracker_comms::TrackerCommsStats {
        let mt = self.session.with_torrents(|torrents| {
            for (_, mt) in torrents {
                if mt.info_hash() == self.info_hash {
                    return Some(mt.clone());
                }
            }
            None
        });
        let mt = match mt {
            Some(mt) => mt,
            None => {
                trace!(info_hash=?self.info_hash, "can't find torrent in the session, using default stats");
                return Default::default();
            }
        };
        let stats = mt.stats();

        use crate::torrent_state::stats::TorrentStatsState as TS;
        use tracker_comms::TrackerCommsStatsState as S;

        tracker_comms::TrackerCommsStats {
            downloaded_bytes: stats.progress_bytes,
            total_bytes: stats.total_bytes,
            uploaded_bytes: stats.uploaded_bytes,
            torrent_state: match stats.state {
                TS::Initializing => S::Initializing,
                TS::Live => S::Live,
                TS::Paused => S::Paused,
                TS::Error => S::None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use buffers::ByteBuf;
    use itertools::Itertools;
    use librqbit_core::torrent_metainfo::{torrent_from_bytes_ext, TorrentMetaV1};

    use super::torrent_file_from_info_bytes;

    #[test]
    fn test_torrent_file_from_info_and_bytes() {
        fn get_trackers(info: &TorrentMetaV1<ByteBuf>) -> Vec<String> {
            info.iter_announce()
                .filter_map(|t| std::str::from_utf8(t.as_ref()).ok().map(|t| t.to_owned()))
                .collect_vec()
        }

        let orig_full_torrent =
            include_bytes!("../resources/ubuntu-21.04-desktop-amd64.iso.torrent");
        let parsed = torrent_from_bytes_ext::<ByteBuf>(&orig_full_torrent[..]).unwrap();
        let parsed_trackers = get_trackers(&parsed.meta);

        let generated_torrent =
            torrent_file_from_info_bytes(parsed.info_bytes.as_ref(), &parsed_trackers).unwrap();
        let generated_parsed =
            torrent_from_bytes_ext::<ByteBuf>(generated_torrent.as_ref()).unwrap();
        assert_eq!(parsed.meta.info_hash, generated_parsed.meta.info_hash);
        assert_eq!(parsed.meta.info, generated_parsed.meta.info);
        assert_eq!(parsed.info_bytes, generated_parsed.info_bytes);
        assert_eq!(parsed_trackers, get_trackers(&generated_parsed.meta));
    }
}
