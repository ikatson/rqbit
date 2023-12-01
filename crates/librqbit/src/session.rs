use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    io::{BufReader, BufWriter, Read},
    net::SocketAddr,
    path::PathBuf,
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use anyhow::{bail, Context};
use bencode::{bencode_serialize_to_writer, BencodeDeserializer};
use buffers::ByteString;
use dht::{Dht, DhtBuilder, Id20, PersistentDht, PersistentDhtConfig, RequestPeersStream};
use librqbit_core::{
    magnet::Magnet,
    peer_id::generate_peer_id,
    torrent_metainfo::{torrent_from_bytes, TorrentMetaV1Info, TorrentMetaV1Owned},
};
use parking_lot::RwLock;
use reqwest::Url;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tracing::{debug, error, error_span, info, warn};

use crate::{
    dht_utils::{read_metainfo_from_peer_receiver, ReadMetainfoResult},
    peer_connection::PeerConnectionOptions,
    spawn_utils::{spawn, BlockingSpawner},
    torrent_state::{ManagedTorrentBuilder, ManagedTorrentHandle, ManagedTorrentState},
};

pub const SUPPORTED_SCHEMES: [&str; 3] = ["http:", "https:", "magnet:"];

pub type TorrentId = usize;

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
            torrents_v2: self
                .torrents
                .iter()
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
                            info: torrent.info().info.clone(),
                            only_files: torrent.only_files.clone(),
                            is_paused: torrent
                                .with_state(|s| matches!(s, ManagedTorrentState::Paused(_))),
                            output_folder: torrent.info().out_dir.clone(),
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
    info: TorrentMetaV1Info<ByteString>,
    trackers: HashSet<String>,
    output_folder: PathBuf,
    only_files: Option<Vec<usize>>,
    is_paused: bool,
}

fn serialize_torrent<S>(t: &TorrentMetaV1Info<ByteString>, serializer: S) -> Result<S::Ok, S::Error>
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

fn deserialize_torrent<'de, D>(deserializer: D) -> Result<TorrentMetaV1Info<ByteString>, D::Error>
where
    D: Deserializer<'de>,
{
    use base64::{engine::general_purpose, Engine as _};
    use serde::de::Error;
    let s = String::deserialize(deserializer)?;
    let b = general_purpose::STANDARD_NO_PAD
        .decode(s)
        .map_err(D::Error::custom)?;
    TorrentMetaV1Info::<ByteString>::deserialize(&mut BencodeDeserializer::new_from_buf(&b))
        .map_err(D::Error::custom)
}

#[derive(Serialize, Deserialize)]
struct SerializedSessionDatabase {
    torrents_v2: HashMap<usize, SerializedTorrent>,
}

pub struct Session {
    peer_id: Id20,
    dht: Option<Dht>,
    persistence_filename: PathBuf,
    peer_opts: PeerConnectionOptions,
    spawner: BlockingSpawner,
    db: RwLock<SessionDatabase>,
    output_folder: PathBuf,
}

async fn torrent_from_url(url: &str) -> anyhow::Result<TorrentMetaV1Owned> {
    let response = reqwest::get(url)
        .await
        .context("error downloading torrent metadata")?;
    if !response.status().is_success() {
        anyhow::bail!("GET {} returned {}", url, response.status())
    }
    let b = response
        .bytes()
        .await
        .with_context(|| format!("error reading response body from {url}"))?;
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
    pub paused: bool,
    pub only_files_regex: Option<String>,
    pub only_files: Option<Vec<usize>>,
    pub overwrite: bool,
    pub list_only: bool,
    pub output_folder: Option<String>,
    pub sub_folder: Option<String>,
    pub peer_opts: Option<PeerConnectionOptions>,
    pub force_tracker_interval: Option<Duration>,
    pub initial_peers: Option<Vec<SocketAddr>>,
    // This is used to restore the session.
    pub preferred_id: Option<usize>,
}

pub struct ListOnlyResponse {
    pub info_hash: Id20,
    pub info: TorrentMetaV1Info<ByteString>,
    pub only_files: Option<Vec<usize>>,
    pub seen_peers: Vec<SocketAddr>,
}

pub enum AddTorrentResponse {
    AlreadyManaged(TorrentId, ManagedTorrentHandle),
    ListOnly(ListOnlyResponse),
    Added(TorrentId, ManagedTorrentHandle),
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
    TorrentInfo(Box<TorrentMetaV1Owned>),
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
            Self::TorrentInfo(_) => unimplemented!(),
        }
    }
}

#[derive(Default)]
pub struct SessionOptions {
    pub disable_dht: bool,
    pub disable_dht_persistence: bool,
    pub persistence: bool,
    // Will default to output_folder/.rqbit-session.json
    pub persistence_filename: Option<PathBuf>,
    pub dht_config: Option<PersistentDhtConfig>,
    pub peer_id: Option<Id20>,
    pub peer_opts: Option<PeerConnectionOptions>,
}

impl Session {
    pub async fn new(
        output_folder: PathBuf,
        spawner: BlockingSpawner,
    ) -> anyhow::Result<Arc<Self>> {
        Self::new_with_opts(output_folder, spawner, SessionOptions::default()).await
    }
    pub async fn new_with_opts(
        output_folder: PathBuf,
        spawner: BlockingSpawner,
        opts: SessionOptions,
    ) -> anyhow::Result<Arc<Self>> {
        let peer_id = opts.peer_id.unwrap_or_else(generate_peer_id);
        let dht = if opts.disable_dht {
            None
        } else {
            let dht = if opts.disable_dht_persistence {
                DhtBuilder::new().await
            } else {
                PersistentDht::create(opts.dht_config).await
            }
            .context("error initializing DHT")?;
            Some(dht)
        };
        let peer_opts = opts.peer_opts.unwrap_or_default();
        let persistence_filename = opts
            .persistence_filename
            .unwrap_or_else(|| output_folder.join(".rqbit-session.json"));
        let session = Arc::new(Self {
            persistence_filename,
            peer_id,
            dht,
            peer_opts,
            spawner,
            output_folder,
            db: RwLock::new(Default::default()),
        });

        if opts.persistence {
            if let Some(parent) = session.persistence_filename.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("couldn't create directory {:?} for session storage", parent)
                })?;
            }
            let session = session.clone();
            spawn(
                "session persistene",
                error_span!("session_persistence"),
                async move {
                    // Populate initial from the state filename
                    if let Err(e) = session.populate_from_stored().await {
                        error!("could not populate session from stored file: {:?}", e);
                    }

                    let session = Arc::downgrade(&session);

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
                },
            );
        }

        Ok(session)
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
        for (id, storrent) in db.torrents_v2.into_iter() {
            let trackers: Vec<ByteString> = storrent
                .trackers
                .into_iter()
                .map(|t| ByteString(t.into_bytes()))
                .collect();
            let info = TorrentMetaV1Owned {
                announce: trackers
                    .get(0)
                    .cloned()
                    .unwrap_or_else(|| ByteString(b"http://retracker.local/announce".into())),
                announce_list: vec![trackers],
                info: storrent.info,
                comment: None,
                created_by: None,
                encoding: None,
                publisher: None,
                publisher_url: None,
                creation_date: None,
                info_hash: Id20::from_str(&storrent.info_hash)?,
            };
            futures.push({
                let session = self.clone();
                async move {
                    session
                        .add_torrent(
                            AddTorrent::TorrentInfo(Box::new(info)),
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
        debug!("wrote persistence to {:?}", &self.persistence_filename);
        Ok(())
    }

    pub fn with_torrents<R>(
        &self,
        callback: impl Fn(&mut dyn Iterator<Item = (TorrentId, &ManagedTorrentHandle)>) -> R,
    ) -> R {
        callback(&mut self.db.read().torrents.iter().map(|(id, t)| (*id, t)))
    }

    pub async fn add_torrent(
        &self,
        add: AddTorrent<'_>,
        opts: Option<AddTorrentOptions>,
    ) -> anyhow::Result<AddTorrentResponse> {
        // Magnet links are different in that we first need to discover the metadata.
        let span = error_span!("add_torrent");
        let _ = span.enter();

        let opts = opts.unwrap_or_default();

        let (info_hash, info, dht_rx, trackers, initial_peers) = match add {
            AddTorrent::Url(magnet) if magnet.starts_with("magnet:") => {
                let Magnet {
                    info_hash,
                    trackers,
                } = Magnet::parse(&magnet).context("provided path is not a valid magnet URL")?;

                let dht_rx = self
                    .dht
                    .as_ref()
                    .context("magnet links without DHT are not supported")?
                    .get_peers(info_hash)?;

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

                debug!("querying DHT for {:?}", info_hash);
                let (info, dht_rx, initial_peers) = match read_metainfo_from_peer_receiver(
                    self.peer_id,
                    info_hash,
                    opts.initial_peers.clone().unwrap_or_default(),
                    dht_rx,
                    Some(self.merge_peer_opts(opts.peer_opts)),
                )
                .await
                {
                    ReadMetainfoResult::Found { info, rx, seen } => (info, rx, seen),
                    ReadMetainfoResult::ChannelClosed { .. } => {
                        anyhow::bail!("DHT died, no way to discover torrent metainfo")
                    }
                };
                debug!("received result from DHT: {:?}", info);
                (
                    info_hash,
                    info,
                    if opts.paused || opts.list_only {
                        None
                    } else {
                        Some(dht_rx)
                    },
                    trackers,
                    initial_peers,
                )
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
                    AddTorrent::TorrentInfo(t) => *t,
                };

                let dht_rx = match self.dht.as_ref() {
                    Some(dht) if !opts.paused && !opts.list_only => {
                        debug!("reading peers for {:?} from DHT", torrent.info_hash);
                        Some(dht.get_peers(torrent.info_hash)?)
                    }
                    _ => None,
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
        dht_peer_rx: Option<RequestPeersStream>,
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
                seen_peers: initial_peers,
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

        {
            let span = managed_torrent.info.span.clone();
            let _ = span.enter();
            managed_torrent
                .start(initial_peers, dht_peer_rx, opts.paused)
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

        let paused = removed
            .with_state_mut(|s| {
                let paused = match s.take() {
                    ManagedTorrentState::Paused(p) => p,
                    ManagedTorrentState::Live(l) => l.pause()?,
                    _ => return Ok(None),
                };
                Ok::<_, anyhow::Error>(Some(paused))
            })
            .context("error pausing torrent");

        match (paused, delete_files) {
            (Err(e), true) => Err(e).context("torrent deleted, but could not delete files"),
            (Err(e), false) => {
                warn!("could not delete torrent files: {:?}", e);
                Ok(())
            }
            (Ok(Some(paused)), true) => {
                drop(paused.files);
                for file in paused.filenames {
                    if let Err(e) = std::fs::remove_file(&file) {
                        warn!("could not delete file {:?}: {:?}", file, e);
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub fn unpause(&self, handle: &ManagedTorrentHandle) -> anyhow::Result<()> {
        let peer_rx = self
            .dht
            .as_ref()
            .map(|dht| dht.get_peers(handle.info_hash()))
            .transpose()?;
        handle.start(Default::default(), peer_rx, false)?;
        Ok(())
    }
}
