use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
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
use log::{debug, error, info, trace, warn};
use parking_lot::{Mutex, RwLock};
use reqwest::Url;
use size_format::SizeFormatterBinary as SF;
use tokio::{
    sync::{mpsc::Sender, Notify, Semaphore},
    time::timeout,
};

use crate::{
    buffers::{ByteBuf, ByteString},
    chunk_tracker::{ChunkMarkingResult, ChunkTracker},
    clone_to_owned::CloneToOwned,
    file_checking::{initial_check, update_hash_from_file},
    lengths::{ChunkInfo, Lengths, ValidPieceIndex},
    peer_binary_protocol::{
        Handshake, Message, MessageBorrowed, MessageDeserializeError, MessageOwned, Piece, Request,
    },
    peer_id::try_decode_peer_id,
    torrent_metainfo::TorrentMetaV1Owned,
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

type PeerHandle = SocketAddr;

enum PeerState {
    Connecting(SocketAddr),
    Live(LivePeerState),
}

type BF = bitvec::vec::BitVec<bitvec::order::Msb0, u8>;

#[derive(Debug, Hash, PartialEq, Eq)]
struct InflightRequest {
    piece: ValidPieceIndex,
    chunk: u32,
}

impl From<&ChunkInfo> for InflightRequest {
    fn from(c: &ChunkInfo) -> Self {
        Self {
            piece: c.piece_index,
            chunk: c.chunk_index,
        }
    }
}

struct LivePeerState {
    #[allow(unused)]
    peer_id: [u8; 20],
    i_am_choked: bool,
    #[allow(unused)]
    peer_choked: bool,
    #[allow(unused)]
    peer_interested: bool,
    outstanding_requests: Arc<Semaphore>,
    have_notify: Arc<Notify>,
    bitfield: Option<BF>,
    inflight_requests: HashSet<InflightRequest>,
}

#[derive(Default)]
struct PeerStates {
    states: HashMap<PeerHandle, PeerState>,
    seen_peers: HashSet<SocketAddr>,
    inflight_pieces: HashSet<ValidPieceIndex>,
    tx: HashMap<PeerHandle, Arc<tokio::sync::mpsc::Sender<MessageOwned>>>,
}

#[derive(Debug, Default)]
struct AggregatePeerStats {
    connecting: usize,
    live: usize,
}

impl PeerStates {
    fn stats(&self) -> AggregatePeerStats {
        self.states
            .values()
            .fold(AggregatePeerStats::default(), |mut s, p| {
                match p {
                    PeerState::Connecting(_) => s.connecting += 1,
                    PeerState::Live(_) => s.live += 1,
                };
                s
            })
    }
    fn add_if_not_seen(
        &mut self,
        addr: SocketAddr,
        tx: tokio::sync::mpsc::Sender<MessageOwned>,
    ) -> Option<PeerHandle> {
        if self.seen_peers.contains(&addr) {
            return None;
        }
        let handle = self.add(addr, tx)?;
        self.seen_peers.insert(addr);
        Some(handle)
    }
    fn get_live(&self, handle: PeerHandle) -> Option<&LivePeerState> {
        if let PeerState::Live(ref l) = self.states.get(&handle)? {
            return Some(l);
        }
        None
    }
    fn get_live_mut(&mut self, handle: PeerHandle) -> Option<&mut LivePeerState> {
        if let PeerState::Live(ref mut l) = self.states.get_mut(&handle)? {
            return Some(l);
        }
        None
    }
    fn try_get_live_mut(&mut self, handle: PeerHandle) -> anyhow::Result<&mut LivePeerState> {
        self.get_live_mut(handle)
            .ok_or_else(|| anyhow::anyhow!("peer dropped"))
    }
    fn add(
        &mut self,
        addr: SocketAddr,
        tx: tokio::sync::mpsc::Sender<MessageOwned>,
    ) -> Option<PeerHandle> {
        let handle = addr;
        if self.states.contains_key(&addr) {
            return None;
        }
        self.states.insert(handle, PeerState::Connecting(addr));
        self.tx.insert(handle, Arc::new(tx));
        Some(handle)
    }
    fn drop_peer(&mut self, handle: PeerHandle) -> Option<PeerState> {
        let result = self.states.remove(&handle);
        self.tx.remove(&handle);
        result
    }
    fn mark_i_am_choked(&mut self, handle: PeerHandle, is_choked: bool) -> Option<bool> {
        match self.states.get_mut(&handle) {
            Some(PeerState::Live(live)) => {
                let prev = live.i_am_choked;
                live.i_am_choked = is_choked;
                return Some(prev);
            }
            _ => return None,
        }
    }
    fn update_bitfield_from_vec(
        &mut self,
        handle: PeerHandle,
        bitfield: Vec<u8>,
    ) -> Option<Option<BF>> {
        match self.states.get_mut(&handle) {
            Some(PeerState::Live(live)) => {
                let bitfield = BF::from_vec(bitfield);
                let prev = live.bitfield.take();
                live.bitfield = Some(bitfield);
                Some(prev)
            }
            _ => None,
        }
    }
    fn clone_tx(&self, handle: PeerHandle) -> Option<Arc<Sender<MessageOwned>>> {
        Some(self.tx.get(&handle)?.clone())
    }
}

struct TorrentManagerInnerLocked {
    peers: PeerStates,
    chunks: ChunkTracker,
}

impl TorrentManagerInnerLocked {}

struct TorrentManagerInner {
    torrent: TorrentMetaV1Owned,
    locked: Arc<RwLock<TorrentManagerInnerLocked>>,
    files: Vec<Arc<Mutex<File>>>,
    info_hash: [u8; 20],
    peer_id: [u8; 20],
    have: AtomicU64,
    downloaded_and_checked: AtomicU64,
    needed: u64,
    uploaded: AtomicU64,
    fetched_bytes: AtomicU64,
    lengths: Lengths,
}

#[derive(Clone)]
struct TorrentManager {
    inner: Arc<TorrentManagerInner>,
}

fn generate_peer_id() -> [u8; 20] {
    let mut peer_id = [0u8; 20];
    let u = uuid::Uuid::new_v4();
    (&mut peer_id[..16]).copy_from_slice(&u.as_bytes()[..]);
    peer_id
}

fn spawn<N: Display + 'static + Send>(
    name: N,
    fut: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
) {
    debug!("starting task \"{}\"", &name);
    tokio::spawn(async move {
        match fut.await {
            Ok(_) => {
                debug!("task \"{}\" finished", &name);
            }
            Err(e) => {
                error!("error in task \"{}\": {:#}", &name, e)
            }
        }
    });
}

fn spawn_blocking<T: Send + Sync + 'static, N: Display + 'static + Send>(
    name: N,
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> tokio::task::JoinHandle<anyhow::Result<T>> {
    debug!("starting blocking task \"{}\"", name);
    tokio::task::spawn_blocking(move || match f() {
        Ok(v) => {
            debug!("blocking task \"{}\" finished", name);
            Ok(v)
        }
        Err(e) => {
            error!("error in blocking task \"{}\": {:#}", name, &e);
            Err(e)
        }
    })
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
            initial_check(&torrent, &files, only_files.as_deref(), &lengths)?;

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
            inner: Arc::new(TorrentManagerInner {
                info_hash: torrent.info_hash,
                torrent,
                peer_id,
                locked: Arc::new(RwLock::new(TorrentManagerInnerLocked {
                    peers: Default::default(),
                    chunks: chunk_tracker,
                })),
                files,
                have: AtomicU64::new(initial_check_results.have_bytes),
                needed: initial_check_results.needed_bytes,
                downloaded_and_checked: Default::default(),
                fetched_bytes: Default::default(),
                uploaded: Default::default(),
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
            let have = self.inner.have.load(Ordering::Relaxed);
            let fetched = self.inner.fetched_bytes.load(Ordering::Relaxed);
            let needed = self.inner.needed;
            let downloaded = self.inner.downloaded_and_checked.load(Ordering::Relaxed);
            let remaining = needed - downloaded;
            let uploaded = self.inner.uploaded.load(Ordering::Relaxed);
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

    async fn on_download_request(
        &self,
        peer_handle: PeerHandle,
        request: Request,
    ) -> anyhow::Result<()> {
        let piece_index = match self.inner.lengths.validate_piece_index(request.index) {
            Some(p) => p,
            None => {
                anyhow::bail!(
                    "{}: received {:?}, but it is not a valid chunk request (piece index is invalid). Ignoring.",
                    peer_handle, request
                );
            }
        };
        let chunk_info = match self.inner.lengths.chunk_info_from_received_data(
            piece_index,
            request.begin,
            request.length,
        ) {
            Some(d) => d,
            None => {
                anyhow::bail!(
                    "{}: received {:?}, but it is not a valid chunk request (chunk data is invalid). Ignoring.",
                    peer_handle, request
                );
            }
        };
        let this = self.clone();

        let clone = this.clone();
        let chunk = spawn_blocking(
            format!(
                "read_chunk_blocking(peer={}, chunk_info={:?}",
                peer_handle, &chunk_info
            ),
            move || clone.read_chunk_blocking(peer_handle, chunk_info),
        )
        .await??;
        let tx = this
            .inner
            .locked
            .read()
            .peers
            .clone_tx(peer_handle)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "peer {} died, dropping chunk that it requested",
                    peer_handle
                )
            })?;
        let message = Message::Piece(Piece::from_vec(
            chunk_info.piece_index.get(),
            chunk_info.offset,
            chunk,
        ));
        info!("sending to {}: {:?}", peer_handle, &message);
        Ok::<_, anyhow::Error>(tx.send(message).await?)
    }
    fn read_chunk_blocking(
        self,
        who_sent: PeerHandle,
        chunk_info: ChunkInfo,
    ) -> anyhow::Result<Vec<u8>> {
        let mut absolute_offset = self.inner.lengths.chunk_absolute_offset(&chunk_info);
        let mut result_buf = vec![0u8; chunk_info.size as usize];
        let mut buf = &mut result_buf[..];

        for (file_idx, file_len) in self.inner.torrent.info.iter_file_lengths().enumerate() {
            if absolute_offset > file_len {
                absolute_offset -= file_len;
                continue;
            }
            let file_remaining_len = file_len - absolute_offset;
            let to_read_in_file = std::cmp::min(file_remaining_len, buf.len() as u64) as usize;

            let mut file_g = self.inner.files[file_idx].lock();
            debug!(
                "piece={}, handle={}, file_idx={}, seeking to {}. To read chunk: {:?}",
                chunk_info.piece_index, who_sent, file_idx, absolute_offset, &chunk_info
            );
            file_g
                .seek(SeekFrom::Start(absolute_offset))
                .with_context(|| {
                    format!(
                        "error seeking to {}, file id: {}",
                        absolute_offset, file_idx
                    )
                })?;
            file_g
                .read_exact(&mut buf[..to_read_in_file])
                .with_context(|| {
                    format!(
                        "error reading {} bytes, file_id: {}",
                        file_idx, to_read_in_file
                    )
                })?;

            buf = &mut buf[to_read_in_file..];

            if buf.is_empty() {
                break;
            }

            absolute_offset = 0;
        }

        return Ok(result_buf);
    }
    fn am_i_interested_in_peer(&self, handle: PeerHandle) -> bool {
        self.get_next_needed_piece(handle).is_some()
    }

    fn on_have(&self, handle: PeerHandle, have: u32) {
        if let Some(bitfield) = self
            .inner
            .locked
            .write()
            .peers
            .get_live_mut(handle)
            .and_then(|l| l.bitfield.as_mut())
        {
            debug!("{}: updated bitfield with have={}", handle, have);
            bitfield.set(have as usize, true)
        }
    }

    async fn on_bitfield(&self, handle: PeerHandle, bitfield: ByteString) -> anyhow::Result<()> {
        if bitfield.len() != self.inner.lengths.piece_bitfield_bytes() as usize {
            anyhow::bail!(
                "dropping {} as its bitfield has unexpected size. Got {}, expected {}",
                handle,
                bitfield.len(),
                self.inner.lengths.piece_bitfield_bytes(),
            );
        }
        self.inner
            .locked
            .write()
            .peers
            .update_bitfield_from_vec(handle, bitfield.0);

        if !self.am_i_interested_in_peer(handle) {
            let tx = self
                .inner
                .locked
                .read()
                .peers
                .clone_tx(handle)
                .ok_or_else(|| anyhow::anyhow!("peer closed"))?;
            tx.send(MessageOwned::Unchoke)
                .await
                .context("peer dropped")?;
            tx.send(MessageOwned::NotInterested)
                .await
                .context("peer dropped")?;
            return Ok(());
        }

        // Additional spawn per peer, not good.
        spawn(
            format!("peer_chunk_requester({})", handle),
            self.clone().task_peer_chunk_requester(handle),
        );
        Ok(())
    }

    async fn task_peer_chunk_requester(self, handle: PeerHandle) -> anyhow::Result<()> {
        let tx = match self.inner.locked.read().peers.clone_tx(handle) {
            Some(tx) => tx,
            None => return Ok(()),
        };
        tx.send(MessageOwned::Unchoke)
            .await
            .context("peer dropped")?;
        tx.send(MessageOwned::Interested)
            .await
            .context("peer dropped")?;

        self.requester(handle).await?;
        Ok::<_, anyhow::Error>(())
    }

    fn on_i_am_choked(&self, handle: PeerHandle) {
        warn!("we are choked by {}", handle);
        self.inner
            .locked
            .write()
            .peers
            .mark_i_am_choked(handle, true);
    }
    fn am_i_choked(&self, peer_handle: PeerHandle) -> Option<bool> {
        self.inner
            .locked
            .read()
            .peers
            .states
            .get(&peer_handle)
            .and_then(|s| match s {
                PeerState::Live(l) => Some(l.i_am_choked),
                _ => None,
            })
    }

    fn try_steal_piece(&self, handle: PeerHandle) -> Option<ValidPieceIndex> {
        let mut rng = rand::thread_rng();
        use rand::seq::IteratorRandom;
        let g = self.inner.locked.read();
        let pl = g.peers.get_live(handle)?;
        g.peers
            .inflight_pieces
            .iter()
            .filter(|p| !pl.inflight_requests.iter().any(|req| req.piece == **p))
            .choose(&mut rng)
            .copied()
    }

    async fn requester(self, handle: PeerHandle) -> anyhow::Result<()> {
        let notify = match self.inner.locked.read().peers.get_live(handle) {
            Some(l) => l.have_notify.clone(),
            None => return Ok(()),
        };

        // TODO: this might dangle, same below.
        #[allow(unused_must_use)]
        {
            timeout(Duration::from_secs(60), notify.notified()).await;
        }

        loop {
            match self.am_i_choked(handle) {
                Some(true) => {
                    warn!("we are choked by {}, can't reserve next piece", handle);
                    #[allow(unused_must_use)]
                    {
                        timeout(Duration::from_secs(60), notify.notified()).await;
                    }
                    continue;
                }
                Some(false) => {}
                None => return Ok(()),
            }

            let next = match self.reserve_next_needed_piece(handle) {
                Some(next) => next,
                None => {
                    if self.get_left_to_download() == 0 {
                        info!("{}: nothing left to download, closing requester", handle);
                        return Ok(());
                    }

                    if let Some(piece) = self.try_steal_piece(handle) {
                        info!("{}: stole a piece {}", handle, piece);
                        piece
                    } else {
                        info!("no pieces to request from {}", handle);
                        #[allow(unused_must_use)]
                        {
                            timeout(Duration::from_secs(60), notify.notified()).await;
                        }
                        continue;
                    }
                }
            };
            let tx = match self.inner.locked.read().peers.clone_tx(handle) {
                Some(tx) => tx,
                None => return Ok(()),
            };
            let sem = match self.inner.locked.read().peers.get_live(handle) {
                Some(live) => live.outstanding_requests.clone(),
                None => return Ok(()),
            };
            for chunk in self.inner.lengths.iter_chunk_infos(next) {
                if self.inner.locked.read().chunks.is_chunk_downloaded(&chunk) {
                    continue;
                }
                if !self
                    .inner
                    .locked
                    .write()
                    .peers
                    .try_get_live_mut(handle)?
                    .inflight_requests
                    .insert(InflightRequest::from(&chunk))
                {
                    warn!(
                        "{}: probably a bug, we already requested {:?}",
                        handle, chunk
                    );
                    continue;
                }

                let request = Request {
                    index: next.get(),
                    begin: chunk.offset,
                    length: chunk.size,
                };
                sem.acquire().await?.forget();

                tx.send(MessageOwned::Request(request))
                    .await
                    .context("peer dropped")?;
            }
        }
    }
    fn on_i_am_unchoked(&self, handle: PeerHandle) {
        debug!("we are unchoked by {}", handle);
        let mut g = self.inner.locked.write();
        let live = match g.peers.get_live_mut(handle) {
            Some(live) => live,
            None => return,
        };
        live.i_am_choked = false;
        live.have_notify.notify_waiters();
        live.outstanding_requests.add_permits(16);
    }
    fn get_next_needed_piece(&self, peer_handle: PeerHandle) -> Option<ValidPieceIndex> {
        let g = self.inner.locked.read();
        let bf = match g.peers.states.get(&peer_handle)? {
            PeerState::Live(l) => l.bitfield.as_ref()?,
            _ => return None,
        };
        for n in g.chunks.get_needed_pieces().iter_ones() {
            if bf.get(n).map(|v| *v) == Some(true) {
                // in theory it should be safe without validation, but whatever.
                return self.inner.lengths.validate_piece_index(n as u32);
            }
        }
        None
    }

    fn reserve_next_needed_piece(&self, peer_handle: PeerHandle) -> Option<ValidPieceIndex> {
        if self.am_i_choked(peer_handle)? {
            warn!("we are choked by {}, can't reserve next piece", peer_handle);
            return None;
        }
        let mut g = self.inner.locked.write();
        let n = {
            let mut n_opt = None;
            let bf = g.peers.get_live(peer_handle)?.bitfield.as_ref()?;
            for n in g.chunks.get_needed_pieces().iter_ones() {
                if bf.get(n).map(|v| *v) == Some(true) {
                    n_opt = Some(n);
                    break;
                }
            }

            self.inner.lengths.validate_piece_index(n_opt? as u32)?
        };
        g.peers.inflight_pieces.insert(n);
        g.chunks.reserve_needed_piece(n);
        Some(n)
    }

    fn check_piece_blocking(
        &self,
        who_sent: PeerHandle,
        piece_index: ValidPieceIndex,
        last_received_chunk: &ChunkInfo,
    ) -> anyhow::Result<bool> {
        let mut h = sha1::Sha1::new();
        let piece_length = self.inner.lengths.piece_length(piece_index);
        let mut absolute_offset = self.inner.lengths.piece_offset(piece_index);
        let mut buf = vec![0u8; std::cmp::min(65536, piece_length as usize)];

        let mut piece_remaining_bytes = piece_length as usize;

        for (file_idx, (name, file_len)) in self
            .inner
            .torrent
            .info
            .iter_filenames_and_lengths()
            .enumerate()
        {
            if absolute_offset > file_len {
                absolute_offset -= file_len;
                continue;
            }
            let file_remaining_len = file_len - absolute_offset;

            let to_read_in_file =
                std::cmp::min(file_remaining_len, piece_remaining_bytes as u64) as usize;
            let mut file_g = self.inner.files[file_idx].lock();
            debug!(
                "piece={}, handle={}, file_idx={}, seeking to {}. Last received chunk: {:?}",
                piece_index, who_sent, file_idx, absolute_offset, &last_received_chunk
            );
            file_g
                .seek(SeekFrom::Start(absolute_offset))
                .with_context(|| {
                    format!(
                        "error seeking to {}, file id: {}",
                        absolute_offset, file_idx
                    )
                })?;
            update_hash_from_file(&mut file_g, &mut h, &mut buf, to_read_in_file).with_context(
                || {
                    format!(
                        "error reading {} bytes, file_id: {} (\"{:?}\")",
                        to_read_in_file, file_idx, name
                    )
                },
            )?;

            piece_remaining_bytes -= to_read_in_file;

            if piece_remaining_bytes == 0 {
                return Ok(true);
            }

            absolute_offset = 0;
        }

        match self.inner.torrent.info.compare_hash(piece_index.get(), &h) {
            Some(true) => {
                debug!("piece={} hash matches", piece_index);
                Ok(true)
            }
            Some(false) => {
                warn!("the piece={} hash does not match", piece_index);
                Ok(false)
            }
            None => {
                // this is probably a bug?
                warn!("compare_hash() did not find the piece");
                anyhow::bail!("compare_hash() did not find the piece");
            }
        }
    }

    // TODO: this is a task per chunk, not good
    async fn task_transmit_haves(self, index: u32) -> anyhow::Result<()> {
        let mut unordered = FuturesUnordered::new();

        for weak in self
            .inner
            .locked
            .read()
            .peers
            .tx
            .values()
            .map(|v| Arc::downgrade(v))
        {
            unordered.push(async move {
                if let Some(tx) = weak.upgrade() {
                    if tx.send(Message::Have(index)).await.is_err() {
                        // whatever
                    }
                }
            });
        }

        while unordered.next().await.is_some() {}
        Ok(())
    }

    fn write_chunk_blocking(
        &self,
        who_sent: PeerHandle,
        data: &Piece<ByteString>,
        chunk_info: &ChunkInfo,
    ) -> anyhow::Result<()> {
        let mut buf = data.block.as_ref();
        let mut absolute_offset = self.inner.lengths.chunk_absolute_offset(&chunk_info);

        for (file_idx, (name, file_len)) in self
            .inner
            .torrent
            .info
            .iter_filenames_and_lengths()
            .enumerate()
        {
            if absolute_offset > file_len {
                absolute_offset -= file_len;
                continue;
            }

            let remaining_len = file_len - absolute_offset;
            let to_write = std::cmp::min(buf.len(), remaining_len as usize);

            let mut file_g = self.inner.files[file_idx].lock();
            debug!(
                "piece={}, chunk={:?}, handle={}, begin={}, file={}, writing {} bytes at {}",
                chunk_info.piece_index,
                chunk_info,
                who_sent,
                chunk_info.offset,
                file_idx,
                to_write,
                absolute_offset
            );
            file_g
                .seek(SeekFrom::Start(absolute_offset))
                .with_context(|| {
                    format!(
                        "error seeking to {} in file {} (\"{:?}\")",
                        absolute_offset, file_idx, name
                    )
                })?;
            file_g
                .write_all(&buf[..to_write])
                .with_context(|| format!("error writing to file {} (\"{:?}\")", file_idx, name))?;
            buf = &buf[to_write..];
            if buf.is_empty() {
                break;
            }

            absolute_offset = 0;
        }

        Ok(())
    }

    fn on_received_piece(
        &self,
        handle: PeerHandle,
        piece: Piece<ByteString>,
    ) -> anyhow::Result<()> {
        let chunk_info = match self.inner.lengths.chunk_info_from_received_piece(&piece) {
            Some(i) => i,
            None => {
                anyhow::bail!(
                    "peer {} sent us a piece that is invalid {:?}",
                    handle,
                    &piece,
                );
            }
        };

        let mut g = self.inner.locked.write();
        let h = g.peers.try_get_live_mut(handle)?;
        h.outstanding_requests.add_permits(1);

        self.inner
            .fetched_bytes
            .fetch_add(piece.block.len() as u64, Ordering::Relaxed);

        if !h
            .inflight_requests
            .remove(&InflightRequest::from(&chunk_info))
        {
            anyhow::bail!(
                "peer {} sent us a piece that we did not ask it for. Requested pieces: {:?}. Got: {:?}", handle, &h.inflight_requests, &piece,
            );
        }

        let should_checksum = match g.chunks.mark_chunk_downloaded(&piece) {
            Some(ChunkMarkingResult::Completed) => {
                debug!(
                    "piece={} done by {}, will write and checksum",
                    piece.index, handle
                );
                // This will prevent others from stealing it.
                g.peers.inflight_pieces.remove(&chunk_info.piece_index);
                true
            }
            Some(ChunkMarkingResult::PreviouslyCompleted) => {
                // TODO: we might need to send cancellations here.
                debug!(
                    "piece={} was done by someone else {}, ignoring",
                    piece.index, handle
                );
                return Ok(());
            }
            Some(ChunkMarkingResult::NotCompleted) => false,
            None => {
                anyhow::bail!(
                    "bogus data received from {}: {:?}, cannot map this to a chunk, dropping peer",
                    handle,
                    piece
                );
            }
        };

        let this = self.clone();

        spawn_blocking(
            format!(
                "write_and_check(piece={}, peer={}, block={:?})",
                piece.index, handle, &piece
            ),
            move || {
                let index = piece.index;

                // TODO: in theory we should unmark the piece as downloaded here. But if there was a disk error, what
                // should we really do? If we unmark it, it will get requested forever...
                this.write_chunk_blocking(handle, &piece, &chunk_info)?;

                if !should_checksum {
                    return Ok(());
                }

                let clone = this.clone();
                match clone
                    .check_piece_blocking(handle, chunk_info.piece_index, &chunk_info)
                    .with_context(|| format!("error checking piece={}", index))?
                {
                    true => {
                        let piece_len =
                            this.inner.lengths.piece_length(chunk_info.piece_index) as u64;
                        this.inner
                            .downloaded_and_checked
                            .fetch_add(piece_len, Ordering::Relaxed);
                        this.inner.have.fetch_add(piece_len, Ordering::Relaxed);
                        this.inner
                            .locked
                            .write()
                            .chunks
                            .mark_piece_downloaded(chunk_info.piece_index);

                        debug!(
                            "piece={} successfully downloaded and verified from {}",
                            index, handle
                        );
                        spawn(
                            "transmit haves",
                            this.clone().task_transmit_haves(piece.index),
                        );
                    }
                    false => {
                        warn!(
                            "checksum for piece={} did not validate, came from {}",
                            index, handle
                        );
                        this.inner
                            .locked
                            .write()
                            .chunks
                            .mark_piece_broken(chunk_info.piece_index);
                    }
                };
                Ok::<_, anyhow::Error>(())
            },
        );
        Ok(())
    }
    fn into_handle(self) -> TorrentManagerHandle {
        TorrentManagerHandle { manager: self }
    }
    fn get_uploaded(&self) -> u64 {
        self.inner.uploaded.load(Ordering::Relaxed)
    }
    fn get_downloaded(&self) -> u64 {
        self.inner.downloaded_and_checked.load(Ordering::Relaxed)
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

    fn get_left_to_download(&self) -> u64 {
        self.inner.needed - self.get_downloaded()
    }

    async fn single_tracker_monitor(self, mut tracker_url: Url) -> anyhow::Result<()> {
        let mut event = Some(TrackerRequestEvent::Started);
        loop {
            let request = TrackerRequest {
                info_hash: self.inner.torrent.info_hash,
                peer_id: self.inner.peer_id,
                port: 6778,
                uploaded: self.get_uploaded(),
                downloaded: self.get_downloaded(),
                left: self.get_left_to_download(),
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
    fn set_peer_live(&self, handle: PeerHandle, h: Handshake) {
        let mut g = self.inner.locked.write();
        match g.peers.states.get_mut(&handle) {
            Some(s @ &mut PeerState::Connecting(_)) => {
                *s = PeerState::Live(LivePeerState {
                    peer_id: h.peer_id,
                    i_am_choked: true,
                    peer_choked: true,
                    peer_interested: false,
                    bitfield: None,
                    have_notify: Arc::new(Notify::new()),
                    outstanding_requests: Arc::new(Semaphore::new(0)),
                    inflight_requests: Default::default(),
                });
            }
            _ => {
                warn!("peer {} was in wrong state", handle);
            }
        }
    }
    async fn manage_peer(
        &self,
        addr: SocketAddr,
        handle: PeerHandle,
        // outgoing_chan_tx: tokio::sync::mpsc::Sender<MessageOwned>,
        mut outgoing_chan: tokio::sync::mpsc::Receiver<MessageOwned>,
    ) -> anyhow::Result<()> {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;
        let mut conn = tokio::net::TcpStream::connect(addr)
            .await
            .context("error connecting")?;
        let handshake = Handshake::new(self.inner.info_hash, self.inner.peer_id);
        conn.write_all(&handshake.serialize())
            .await
            .context("error writing handshake")?;
        let mut read_buf = vec![0u8; 16384 * 2];
        let read_bytes = conn
            .read(&mut read_buf)
            .await
            .context("error reading handshake")?;
        if read_bytes == 0 {
            anyhow::bail!("bad handshake");
        }
        let (h, hlen) = Handshake::deserialize(&read_buf[..read_bytes])
            .map_err(|e| anyhow::anyhow!("error deserializing handshake: {:?}", e))?;

        let mut read_so_far = 0usize;
        debug!(
            "connected peer {}: {:?}",
            addr,
            try_decode_peer_id(h.peer_id)
        );
        if h.info_hash != self.inner.info_hash {
            anyhow::bail!("info hash does not match");
        }

        self.set_peer_live(handle, h);

        if read_bytes > hlen {
            read_buf.copy_within(hlen..read_bytes, 0);
            read_so_far = read_bytes - hlen;
        }

        let (mut read_half, mut write_half) = tokio::io::split(conn);

        let this = self.clone();
        let writer = async move {
            let mut buf = Vec::<u8>::new();
            let keep_alive_interval = Duration::from_secs(120);

            if this.inner.have.load(Ordering::Relaxed) > 0 {
                let len = {
                    let g = this.inner.locked.read();
                    let msg = Message::Bitfield(ByteBuf(g.chunks.get_have_pieces().as_raw_slice()));
                    let len = msg.serialize(&mut buf);
                    debug!("sending to {}: {:?}, length={}", handle, &msg, len);
                    len
                };

                write_half
                    .write_all(&buf[..len])
                    .await
                    .context("error writing bitfield to peer")?;
                debug!("sent bitfield to {}", handle);
            }

            loop {
                let msg = match timeout(keep_alive_interval, outgoing_chan.recv()).await {
                    Ok(Some(msg)) => msg,
                    Ok(None) => {
                        anyhow::bail!("closing writer, channel closed")
                    }
                    Err(_) => MessageOwned::KeepAlive,
                };

                let uploaded_add = match &msg {
                    Message::Piece(p) => Some(p.block.len()),
                    _ => None,
                };

                let len = msg.serialize(&mut buf);
                debug!("sending to {}: {:?}, length={}", handle, &msg, len);

                write_half
                    .write_all(&buf[..len])
                    .await
                    .context("error writing the message to peer")?;

                if let Some(uploaded_add) = uploaded_add {
                    this.inner
                        .uploaded
                        .fetch_add(uploaded_add as u64, Ordering::Relaxed);
                }
            }

            // For type inference.
            #[allow(unreachable_code)]
            Ok::<_, anyhow::Error>(())
        };

        let reader = async move {
            loop {
                let message = loop {
                    match MessageBorrowed::deserialize(&read_buf[..read_so_far]) {
                        Ok((msg, size)) => {
                            let msg = msg.clone_to_owned();
                            if read_so_far > size {
                                read_buf.copy_within(size..read_so_far, 0);
                            }
                            read_so_far -= size;
                            break msg;
                        }
                        Err(MessageDeserializeError::NotEnoughData(d, _)) => {
                            if read_buf.len() < read_so_far + d {
                                read_buf.reserve(d);
                                read_buf.resize(read_buf.capacity(), 0);
                            }

                            let size = read_half
                                .read(&mut read_buf[read_so_far..])
                                .await
                                .context("error reading from peer")?;
                            if size == 0 {
                                anyhow::bail!(
                                    "disconnected while reading, read so far: {}",
                                    read_so_far
                                )
                            }
                            read_so_far += size;
                        }
                        Err(e) => return Err(e.into()),
                    }
                };

                trace!("received from {}: {:?}", handle, &message);

                match message {
                    Message::Request(request) => {
                        self.on_download_request(handle, request)
                            .await
                            .with_context(|| {
                                format!("error handling download request from {}", handle)
                            })?;
                    }
                    Message::Bitfield(b) => self.on_bitfield(handle, b).await?,
                    Message::Choke => self.on_i_am_choked(handle),
                    Message::Unchoke => self.on_i_am_unchoked(handle),
                    Message::Interested => {
                        warn!(
                            "{} is interested, but support for interested messages not implemented",
                            handle
                        )
                    }
                    Message::Piece(piece) => {
                        self.on_received_piece(handle, piece)
                            .context("error in on_received_piece()")?;
                    }
                    Message::KeepAlive => {
                        debug!("keepalive received from {}", handle);
                    }
                    Message::Have(h) => self.on_have(handle, h),
                    Message::NotInterested => {
                        info!("received \"not interested\", but we don't care yet")
                    }
                }
            }

            // For type inference.
            #[allow(unreachable_code)]
            Ok::<_, anyhow::Error>(())
        };

        let r = tokio::select! {
            r = reader => {r}
            r = writer => {r}
        };
        debug!("{}: either reader or writer are done, exiting", handle);
        r
    }
    fn drop_peer(&self, handle: PeerHandle) -> bool {
        let mut g = self.inner.locked.write();
        let peer = match g.peers.drop_peer(handle) {
            Some(peer) => peer,
            None => return false,
        };
        match peer {
            PeerState::Connecting(_) => {}
            PeerState::Live(l) => {
                for req in l.inflight_requests {
                    g.chunks.mark_chunk_request_cancelled(req.piece, req.chunk);
                }
            }
        }
        true
    }
    fn add_peer(&self, addr: SocketAddr) {
        let (out_tx, out_rx) = tokio::sync::mpsc::channel::<MessageOwned>(1);
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

        let this = self.clone();
        spawn(format!("manage_peer({})", handle), async move {
            if let Err(e) = this.manage_peer(addr, handle, out_rx).await {
                error!("error managing peer, will drop {}: {:#}", handle, e)
            };
            this.drop_peer(handle);
            Ok::<_, anyhow::Error>(())
        });
    }
}
