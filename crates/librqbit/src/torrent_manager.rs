use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
    fs::{File, OpenOptions},
    io::{Read, Seek, Write},
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
use tokio::sync::{mpsc::Sender, Notify, Semaphore};

use crate::{
    buffers::ByteString,
    chunk_tracker::ChunkTracker,
    clone_to_owned::CloneToOwned,
    lengths::{Lengths, ValidPieceIndex},
    peer_comms::{
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
}

impl TorrentManagerBuilder {
    pub fn new<P: AsRef<Path>>(torrent: TorrentMetaV1Owned, output_folder: P) -> Self {
        Self {
            torrent,
            overwrite: false,
            output_folder: output_folder.as_ref().into(),
        }
    }

    pub fn overwrite(mut self, overwrite: bool) -> Self {
        self.overwrite = overwrite;
        self
    }

    pub async fn start_manager(self) -> anyhow::Result<TorrentManagerHandle> {
        TorrentManager::start(self.torrent, self.output_folder, self.overwrite)
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
    requested_pieces: HashSet<ValidPieceIndex>,
}

#[derive(Default)]
struct PeerStates {
    states: HashMap<PeerHandle, PeerState>,
    seen_peers: HashSet<SocketAddr>,
    tx: HashMap<PeerHandle, Arc<tokio::sync::mpsc::Sender<MessageOwned>>>,
}

impl PeerStates {
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
    fn mark_peer_choked(&mut self, handle: PeerHandle, is_choked: bool) -> Option<bool> {
        match self.states.get_mut(&handle) {
            Some(PeerState::Live(live)) => {
                let prev = live.peer_choked;
                live.peer_choked = is_choked;
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
    fn get_tx(&self, handle: PeerHandle) -> Option<&Sender<MessageOwned>> {
        self.tx.get(&handle).map(|v| v.as_ref())
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
    incoming_tx: tokio::sync::mpsc::Sender<(PeerHandle, MessageOwned)>,
    downloaded: AtomicU64,
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
    debug!("starting task \"{}\"", name);
    tokio::spawn(async move {
        match fut.await {
            Ok(_) => {
                debug!("task \"{}\" finished", name);
            }
            Err(e) => {
                error!("error in task \"{}\": {:#}", name, e)
            }
        }
    });
}

fn spawn_blocking<N: Display + 'static + Send>(
    name: N,
    f: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
) {
    debug!("starting blocking task \"{}\"", name);
    tokio::task::spawn_blocking(move || match f() {
        Ok(_) => {
            debug!("blocking task \"{}\" finished", name);
        }
        Err(e) => {
            error!("error in blocking task \"{}\": {:#}", name, e)
        }
    });
}

fn make_lengths(torrent: &TorrentMetaV1Owned) -> anyhow::Result<Lengths> {
    let total_length = torrent.info.iter_file_lengths().sum();
    Lengths::new(total_length, torrent.info.piece_length, None)
}

fn compute_needed_pieces(
    torrent: &TorrentMetaV1Owned,
    files: &mut [Arc<Mutex<File>>],
    lengths: &Lengths,
) -> anyhow::Result<BF> {
    let needed_pieces = vec![u8::MAX; lengths.piece_bitfield_bytes()];
    let needed_pieces = BF::from_vec(needed_pieces);

    // TODO: read and validate existing files
    Ok(needed_pieces)
}

impl TorrentManager {
    pub fn start<P: AsRef<Path>>(
        torrent: TorrentMetaV1Owned,
        out: P,
        overwrite: bool,
    ) -> anyhow::Result<TorrentManagerHandle> {
        let mut files = {
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
        let needed_pieces = compute_needed_pieces(&torrent, &mut files, &lengths)?;
        debug!("computed lengths: {:?}", &lengths);
        let chunk_tracker = ChunkTracker::new(needed_pieces, lengths);

        let (incoming_tx, incoming_rx) =
            tokio::sync::mpsc::channel::<(PeerHandle, MessageOwned)>(1);

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
                incoming_tx,
                downloaded: Default::default(),
                fetched_bytes: Default::default(),
                uploaded: Default::default(),
                lengths,
            }),
        };

        spawn("tracker_monitor", mgr.clone().task_tracker_monitor());
        spawn(
            "incoming_rx_handler",
            mgr.clone().task_incoming_rx_handler(incoming_rx),
        );
        spawn("Stats printer", mgr.clone().stats_printer());
        Ok(mgr.into_handle())
    }

    async fn stats_printer(self) -> anyhow::Result<()> {
        loop {
            let live_peers = self.inner.locked.read().peers.states.len();
            let downloaded_bytes = self.inner.downloaded.load(Ordering::Relaxed);
            let downloaded = self.inner.downloaded.load(Ordering::Relaxed) / 1024 / 1024;
            let fetched = self.inner.fetched_bytes.load(Ordering::Relaxed) / 1024 / 1024;
            let total_length = self.inner.lengths.total_length();
            let pct = if total_length == downloaded {
                100f64
            } else {
                (downloaded_bytes as f64 / self.inner.lengths.total_length() as f64) * 100f64
            };
            info!(
                "Total downloaded and checked {}MiB ({:.2}%), fetched {}MiB, live peers={}",
                downloaded, pct, fetched, live_peers
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
    async fn task_incoming_rx_handler(
        self,
        mut incoming_tx: tokio::sync::mpsc::Receiver<(PeerHandle, MessageOwned)>,
    ) -> anyhow::Result<()> {
        loop {
            let (peer_handle, message): (PeerHandle, MessageOwned) = match incoming_tx.recv().await
            {
                Some(msg) => msg,
                None => {
                    return Ok(());
                }
            };

            match message {
                Message::Request(request) => {
                    warn!(
                        "{}: received {:?} , but download requests not implemented",
                        peer_handle, request
                    )
                }
                Message::Bitfield(b) => self.on_bitfield(peer_handle, b),
                Message::Choke => self.on_i_am_choked(peer_handle),
                Message::Unchoke => self.on_i_am_unchoked(peer_handle),
                Message::Interested => {
                    warn!(
                        "{} is interested, but support for interested messages not implemented",
                        peer_handle
                    )
                }
                Message::Piece(piece) => {
                    self.on_received_piece(peer_handle, piece);
                }
                Message::KeepAlive => {
                    debug!("keepalive received from {}", peer_handle);
                }
                Message::Have(h) => self.on_have(peer_handle, h),
                Message::NotInterested => {
                    info!("received \"not interested\", but we don't care yet")
                }
            }
        }
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

    fn on_bitfield(&self, handle: PeerHandle, bitfield: ByteString) {
        if bitfield.len() != self.inner.lengths.piece_bitfield_bytes() as usize {
            warn!(
                "dropping {} as its bitfield has unexpected size. Got {}, expected {}",
                handle,
                bitfield.len(),
                self.inner.lengths.piece_bitfield_bytes(),
            );
            self.inner.locked.write().peers.drop_peer(handle);
            return;
        }
        self.inner
            .locked
            .write()
            .peers
            .update_bitfield_from_vec(handle, bitfield.0);
        if !self.am_i_interested_in_peer(handle) {
            self.inner.locked.write().peers.drop_peer(handle);
            return;
        }

        // Additional spawn per peer.
        spawn(
            format!("peer_chunk_requester({})", handle),
            self.clone().task_peer_chunk_requester(handle),
        );
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

    async fn requester(self, handle: PeerHandle) -> anyhow::Result<()> {
        let notify = match self.inner.locked.read().peers.get_live(handle) {
            Some(l) => l.have_notify.clone(),
            None => return Ok(()),
        };
        // TODO: this might dangle
        tokio::time::timeout(Duration::from_secs(60), notify.notified()).await;

        loop {
            let next = match self.reserve_next_needed_piece(handle) {
                Some(next) => next,
                None => {
                    info!("no pieces to request from {}", handle);
                    let notify = match self.inner.locked.read().peers.get_live(handle) {
                        Some(l) => l.have_notify.clone(),
                        None => return Ok(()),
                    };
                    // TODO: this might dangle
                    tokio::time::timeout(Duration::from_secs(60), notify.notified()).await;
                    continue;
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

        g.peers
            .get_live_mut(peer_handle)?
            .requested_pieces
            .insert(n);
        g.chunks.reserve_needed_piece(n);
        Some(n)
    }

    fn check_piece_blocking(
        &self,
        who_sent: PeerHandle,
        index: ValidPieceIndex,
    ) -> anyhow::Result<bool> {
        let mut h = sha1::Sha1::new();
        let piece_length = self.inner.lengths.piece_length(index);
        let mut absolute_offset = self.inner.lengths.piece_offset(index);
        let mut buf = vec![0; std::cmp::min(8192, piece_length as usize)];

        let mut left_to_read = piece_length as usize;

        for (file_idx, file_len) in self.inner.torrent.info.iter_file_lengths().enumerate() {
            if absolute_offset > file_len {
                absolute_offset -= file_len;
                continue;
            }
            let file_remaining_len = file_len - absolute_offset;

            let mut left_to_read_in_file =
                std::cmp::min(file_remaining_len, left_to_read as u64) as usize;
            let mut file_g = self.inner.files[file_idx].lock();
            trace!("piece={}, seeking to {}", index, absolute_offset);
            file_g
                .seek(std::io::SeekFrom::Start(absolute_offset))
                .with_context(|| {
                    format!(
                        "error seeking to {}, file id: {}",
                        absolute_offset, file_idx
                    )
                })?;
            while left_to_read_in_file > 0 {
                let chunk_length = std::cmp::min(buf.len(), left_to_read_in_file);
                file_g
                    .read_exact(&mut buf[..chunk_length])
                    .with_context(|| {
                        format!(
                            "error reading {} bytes, file_id: {}, left_to_read_in_file: {}",
                            chunk_length, file_idx, left_to_read_in_file
                        )
                    })?;
                h.update(&buf[..chunk_length]);
                left_to_read_in_file -= chunk_length;
            }

            match self.inner.torrent.info.compare_hash(index.get(), &h) {
                Some(true) => {
                    debug!("piece={} hash matches", index);
                }
                Some(false) => {
                    warn!("the piece={} hash does not match", index);
                    return Ok(false);
                }
                None => {
                    // this is probably a bug?
                    warn!("compare_hash() did not find the piece");
                    anyhow::bail!("compare_hash() did not find the piece");
                }
            }

            left_to_read -= left_to_read_in_file;

            if left_to_read == 0 {
                return Ok(true);
            }

            absolute_offset = 0;
        }
        Ok(true)
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
        chunk: &Piece<ByteString>,
    ) -> anyhow::Result<()> {
        let mut absolute_offset =
            self.inner.torrent.info.piece_length as u64 * chunk.index as u64 + chunk.begin as u64;

        let mut buf = chunk.block.as_ref();

        for (file_idx, file_len) in self.inner.torrent.info.iter_file_lengths().enumerate() {
            if absolute_offset > file_len {
                absolute_offset -= file_len;
                continue;
            }

            let remaining_len = file_len - absolute_offset;
            let to_write = std::cmp::min(buf.len(), remaining_len as usize);

            let mut file_g = self.inner.files[file_idx].lock();
            debug!(
                "piece={}, handle={}, writing {} bytes to file {} at offset {}",
                chunk.index, who_sent, to_write, file_idx, absolute_offset
            );
            debug!("piece={}, seeking to {}", chunk.index, absolute_offset);
            file_g.seek(std::io::SeekFrom::Start(absolute_offset))?;
            file_g.write_all(&buf[..to_write])?;
            buf = &buf[to_write..];
            if buf.is_empty() {
                break;
            }

            absolute_offset = 0;
        }

        Ok(())
    }

    fn on_received_piece(&self, handle: PeerHandle, piece: Piece<ByteString>) -> Option<()> {
        let chunk_info = match self
            .inner
            .lengths
            .chunk_info_from_received_piece_data(&piece)
        {
            Some(i) => i,
            None => {
                warn!(
                    "peer {} sent us a piece that is invalid {:?}, dropping",
                    handle, &piece,
                );
                self.drop_peer(handle);
                return None;
            }
        };

        let mut g = self.inner.locked.write();
        let h = g.peers.get_live_mut(handle)?;
        h.outstanding_requests.add_permits(1);

        self.inner
            .fetched_bytes
            .fetch_add(piece.block.len() as u64, Ordering::Relaxed);

        if !h.requested_pieces.contains(&chunk_info.piece_index) {
            warn!(
                "peer {} sent us a piece that we did not ask for, dropping it. Requested pieces: {:?}. Got: {:?}", handle, &h.requested_pieces, &piece,
            );
            self.drop_peer(handle);
            return None;
        }

        let this = self.clone();
        spawn_blocking(
            format!("write_and_check(piece={}, block={:?})", piece.index, &piece),
            move || {
                let index = piece.index;
                this.write_chunk_blocking(handle, &piece)?;

                let piece_done = match this
                    .inner
                    .locked
                    .write()
                    .chunks
                    .mark_chunk_downloaded(&piece)
                {
                    Some(true) => {
                        debug!(
                            "piece={} done, requesting a piece from {}",
                            piece.index, handle
                        );
                        true
                    }
                    Some(false) => false,
                    None => {
                        warn!(
                            "bogus data received from {}: {:?}, cannot map this to a chunk, dropping peer",
                            handle, piece
                        );
                        this.drop_peer(handle);
                        return Ok(());
                    }
                };

                if !piece_done {
                    return Ok(());
                }
                // Ignore responses about this piece from now on.
                this.inner
                    .locked
                    .write()
                    .peers
                    .get_live_mut(handle)
                    .map(|l| l.requested_pieces.remove(&chunk_info.piece_index));

                let clone = this.clone();
                match clone
                    .check_piece_blocking(handle, chunk_info.piece_index)
                    .with_context(|| format!("error checking piece={}", index))?
                {
                    true => {
                        this.inner.downloaded.fetch_add(
                            this.inner.lengths.piece_length(chunk_info.piece_index) as u64,
                            Ordering::Relaxed,
                        );
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
                            .mark_piece_needed(chunk_info.piece_index);
                        // this.drop_peer(handle);
                    }
                };
                Ok::<_, anyhow::Error>(())
            },
        );
        Some(())
    }
    fn into_handle(self) -> TorrentManagerHandle {
        TorrentManagerHandle { manager: self }
    }
    fn get_uploaded(&self) -> u64 {
        self.inner.uploaded.load(Ordering::Relaxed)
    }
    fn get_downloaded(&self) -> u64 {
        self.inner.downloaded.load(Ordering::Relaxed)
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
    fn get_total(&self) -> u64 {
        if let Some(length) = self.inner.torrent.info.length {
            return length;
        }
        self.inner
            .torrent
            .info
            .files
            .as_ref()
            .map(|files| files.iter().map(|f| f.length).sum())
            .unwrap_or_default()
    }
    fn get_left_to_download(&self) -> u64 {
        self.get_total() - self.get_downloaded()
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
    fn set_peer_live(&self, handle: PeerHandle, addr: SocketAddr, h: Handshake) {
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
                    requested_pieces: Default::default(),
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
        incoming_chan: tokio::sync::mpsc::Sender<(PeerHandle, MessageOwned)>,
        // outgoing_chan_tx: tokio::sync::mpsc::Sender<MessageOwned>,
        mut outgoing_chan: tokio::sync::mpsc::Receiver<MessageOwned>,
    ) -> anyhow::Result<()> {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;
        let mut conn = tokio::net::TcpStream::connect(addr).await?;
        let handshake = Handshake::new(self.inner.info_hash, self.inner.peer_id);
        conn.write_all(&handshake.serialize()).await?;
        let mut read_buf = vec![0u8; 16384 * 2];
        let read_bytes = conn.read(&mut read_buf).await?;
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

        self.set_peer_live(handle, addr, h);

        if read_bytes > hlen {
            read_buf.copy_within(hlen..read_bytes, 0);
            read_so_far = read_bytes - hlen;
        }

        let (mut read_half, mut write_half) = tokio::io::split(conn);

        let writer = async move {
            let mut buf = vec![0u8; 1024];
            let keep_alive_interval = Duration::from_secs(120);
            loop {
                let msg =
                    match tokio::time::timeout(keep_alive_interval, outgoing_chan.recv()).await {
                        Ok(Some(msg)) => msg,
                        Ok(None) => return Err(anyhow::anyhow!("torrent manager closed")),
                        Err(_) => MessageOwned::KeepAlive,
                    };

                let len = msg.serialize(&mut buf);
                debug!("sending to {}: {:?}, length={}", handle, &msg, len);

                write_half
                    .write_all(&buf[..len])
                    .await
                    .context("error writing")?;
            }

            // For type inference.
            #[allow(unreachable_code)]
            Ok::<_, anyhow::Error>(())
        };

        let reader = async move {
            loop {
                let (message, size) = loop {
                    match MessageBorrowed::deserialize(&read_buf[..read_so_far]) {
                        Ok((msg, size)) => break (msg.clone_to_owned(), size),
                        Err(MessageDeserializeError::NotEnoughData(d, _)) => {
                            if read_buf.len() < read_so_far + d {
                                read_buf.reserve(d);
                                read_buf.resize(read_buf.capacity(), 0);
                            }
                        }
                        Err(e) => return Err(e.into()),
                    }

                    let size = read_half
                        .read(&mut read_buf[read_so_far..])
                        .await
                        .context("error reading from peer")?;
                    if size == 0 {
                        anyhow::bail!("disconnected while reading, read so far: {}", read_so_far)
                    }
                    read_so_far += size;
                };

                if read_so_far > size {
                    read_buf.copy_within(size..read_so_far, 0);
                }
                read_so_far -= size;

                incoming_chan
                    .send((handle, message))
                    .await
                    .context("error sending received message")?;
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
                for piece in l.requested_pieces {
                    g.chunks.mark_piece_needed(piece);
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
            if let Err(e) = this
                .manage_peer(addr, handle, this.inner.incoming_tx.clone(), out_rx)
                .await
            {
                error!("error managing peer, will drop {}: {:#}", handle, e)
            };
            this.drop_peer(handle);
            Ok::<_, anyhow::Error>(())
        });
    }
}
