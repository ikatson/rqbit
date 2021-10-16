use std::{
    collections::{HashMap, HashSet},
    fs::File,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::Context;
use buffers::{ByteBuf, ByteString};
use clone_to_owned::CloneToOwned;
use futures::{stream::FuturesUnordered, StreamExt};
use librqbit_core::{
    id20::Id20,
    lengths::{ChunkInfo, Lengths, ValidPieceIndex},
    torrent_metainfo::TorrentMetaV1Info,
};
use log::{debug, info, trace, warn};
use parking_lot::{Mutex, RwLock, RwLockReadGuard};
use peer_binary_protocol::{
    extended::handshake::ExtendedHandshake, Handshake, Message, MessageOwned, Piece, Request,
};
use serde::Serialize;
use sha1w::Sha1;
use tokio::{
    sync::{
        mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
        Semaphore,
    },
    time::timeout,
};

use crate::{
    chunk_tracker::{ChunkMarkingResult, ChunkTracker},
    file_ops::FileOps,
    peer_connection::{
        PeerConnection, PeerConnectionHandler, PeerConnectionOptions, WriterRequest,
    },
    peer_state::{InflightRequest, LivePeerState, PeerState},
    spawn_utils::{spawn, BlockingSpawner},
    type_aliases::{PeerHandle, BF},
};

pub struct InflightPiece {
    pub peer: PeerHandle,
    pub started: Instant,
}

#[derive(Default)]
pub struct PeerStates {
    states: HashMap<PeerHandle, PeerState>,
    seen: HashSet<SocketAddr>,
    inflight_pieces: HashMap<ValidPieceIndex, InflightPiece>,
    tx: HashMap<PeerHandle, Arc<tokio::sync::mpsc::UnboundedSender<WriterRequest>>>,
}

#[derive(Debug, Default)]
pub struct AggregatePeerStats {
    pub queued: usize,
    pub connecting: usize,
    pub live: usize,
    pub seen: usize,
}

impl PeerStates {
    pub fn stats(&self) -> AggregatePeerStats {
        let mut stats = self
            .states
            .values()
            .fold(AggregatePeerStats::default(), |mut s, p| {
                match p {
                    PeerState::Connecting => s.connecting += 1,
                    PeerState::Live(_) => s.live += 1,
                    PeerState::Queued => s.queued += 1,
                };
                s
            });
        stats.seen = self.seen.len();
        stats
    }
    pub fn add_if_not_seen(
        &mut self,
        addr: SocketAddr,
        tx: UnboundedSender<WriterRequest>,
    ) -> Option<PeerHandle> {
        if self.seen.contains(&addr) {
            return None;
        }
        let handle = self.add(addr, tx)?;
        self.seen.insert(addr);
        Some(handle)
    }
    pub fn seen(&self) -> &HashSet<SocketAddr> {
        &self.seen
    }
    pub fn get_live(&self, handle: PeerHandle) -> Option<&LivePeerState> {
        if let PeerState::Live(ref l) = self.states.get(&handle)? {
            return Some(l);
        }
        None
    }
    pub fn get_live_mut(&mut self, handle: PeerHandle) -> Option<&mut LivePeerState> {
        if let PeerState::Live(ref mut l) = self.states.get_mut(&handle)? {
            return Some(l);
        }
        None
    }
    pub fn try_get_live_mut(&mut self, handle: PeerHandle) -> anyhow::Result<&mut LivePeerState> {
        self.get_live_mut(handle)
            .ok_or_else(|| anyhow::anyhow!("peer dropped"))
    }
    pub fn add(
        &mut self,
        addr: SocketAddr,
        tx: UnboundedSender<WriterRequest>,
    ) -> Option<PeerHandle> {
        let handle = addr;
        if self.states.contains_key(&addr) {
            return None;
        }
        self.states.insert(handle, PeerState::Queued);
        self.tx.insert(handle, Arc::new(tx));
        Some(handle)
    }
    pub fn drop_peer(&mut self, handle: PeerHandle) -> Option<PeerState> {
        let result = self.states.remove(&handle);
        self.tx.remove(&handle);
        result
    }
    pub fn mark_i_am_choked(&mut self, handle: PeerHandle, is_choked: bool) -> Option<bool> {
        let live = self.get_live_mut(handle)?;
        let prev = live.i_am_choked;
        live.i_am_choked = is_choked;
        Some(prev)
    }
    pub fn mark_peer_interested(
        &mut self,
        handle: PeerHandle,
        is_interested: bool,
    ) -> Option<bool> {
        let live = self.get_live_mut(handle)?;
        let prev = live.peer_interested;
        live.peer_interested = is_interested;
        Some(prev)
    }
    pub fn update_bitfield_from_vec(
        &mut self,
        handle: PeerHandle,
        bitfield: Vec<u8>,
    ) -> Option<Option<BF>> {
        let live = self.get_live_mut(handle)?;
        let bitfield = BF::from_vec(bitfield);
        let prev = live.bitfield.take();
        live.bitfield = Some(bitfield);
        Some(prev)
    }
    pub fn clone_tx(&self, handle: PeerHandle) -> Option<Arc<UnboundedSender<WriterRequest>>> {
        Some(self.tx.get(&handle)?.clone())
    }
    pub fn remove_inflight_piece(&mut self, piece: ValidPieceIndex) -> Option<InflightPiece> {
        self.inflight_pieces.remove(&piece)
    }
}

pub struct TorrentStateLocked {
    pub peers: PeerStates,
    pub chunks: ChunkTracker,
}

#[derive(Default, Debug)]
struct AtomicStats {
    have: AtomicU64,
    downloaded_and_checked: AtomicU64,
    uploaded: AtomicU64,
    fetched_bytes: AtomicU64,

    downloaded_pieces: AtomicU64,
    total_piece_download_ms: AtomicU64,
}

impl AtomicStats {
    fn average_piece_download_time(&self) -> Option<Duration> {
        let d = self.downloaded_pieces.load(Ordering::Relaxed);
        let t = self.total_piece_download_ms.load(Ordering::Relaxed);
        if d == 0 {
            return None;
        }
        Some(Duration::from_secs_f64(t as f64 / d as f64 / 1000f64))
    }
}

#[derive(Debug, Serialize)]
pub struct StatsSnapshot {
    pub have_bytes: u64,
    pub downloaded_and_checked_bytes: u64,
    pub downloaded_and_checked_pieces: u64,
    pub fetched_bytes: u64,
    pub uploaded_bytes: u64,
    pub initially_needed_bytes: u64,
    pub remaining_bytes: u64,
    pub total_bytes: u64,
    pub live_peers: u32,
    pub seen_peers: u32,
    pub connecting_peers: u32,
    #[serde(skip)]
    pub time: Instant,
    pub queued_peers: u32,
    total_piece_download_ms: u64,
}

impl StatsSnapshot {
    pub fn average_piece_download_time(&self) -> Option<Duration> {
        let d = self.downloaded_and_checked_pieces;
        let t = self.total_piece_download_ms;
        if d == 0 {
            return None;
        }
        Some(Duration::from_secs_f64(t as f64 / d as f64 / 1000f64))
    }
}

#[derive(Default)]
pub struct TorrentStateOptions {
    pub peer_connect_timeout: Option<Duration>,
}

pub struct TorrentState {
    info: TorrentMetaV1Info<ByteString>,
    locked: Arc<RwLock<TorrentStateLocked>>,
    files: Vec<Arc<Mutex<File>>>,
    info_hash: Id20,
    peer_id: Id20,
    lengths: Lengths,
    needed: u64,
    have_plus_needed: u64,
    stats: AtomicStats,
    options: TorrentStateOptions,

    peer_semaphore: Semaphore,
    peer_queue_tx: UnboundedSender<(SocketAddr, UnboundedReceiver<WriterRequest>)>,
}

impl TorrentState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        info: TorrentMetaV1Info<ByteString>,
        info_hash: Id20,
        peer_id: Id20,
        files: Vec<Arc<Mutex<File>>>,
        chunk_tracker: ChunkTracker,
        lengths: Lengths,
        have_bytes: u64,
        needed_bytes: u64,
        spawner: BlockingSpawner,
        options: Option<TorrentStateOptions>,
    ) -> Arc<Self> {
        let options = options.unwrap_or_default();
        let (peer_queue_tx, mut peer_queue_rx) = unbounded_channel();
        let state = Arc::new(TorrentState {
            info_hash,
            info,
            peer_id,
            locked: Arc::new(RwLock::new(TorrentStateLocked {
                peers: Default::default(),
                chunks: chunk_tracker,
            })),
            files,
            stats: AtomicStats {
                have: AtomicU64::new(have_bytes),
                ..Default::default()
            },
            needed: needed_bytes,
            have_plus_needed: needed_bytes + have_bytes,
            lengths,
            options,

            peer_semaphore: Semaphore::new(128),
            peer_queue_tx,
        });
        spawn("peer adder", {
            let state = state.clone();
            async move {
                loop {
                    let (addr, out_rx) = peer_queue_rx.recv().await.unwrap();

                    match state.locked.write().peers.states.get_mut(&addr) {
                        Some(s @ PeerState::Queued) => *s = PeerState::Connecting,
                        s => {
                            warn!("did not expect to see the peer in state {:?}", s);
                            continue;
                        }
                    };

                    state.peer_semaphore.acquire().await.unwrap().forget();

                    let handler = PeerHandler {
                        addr,
                        state: state.clone(),
                        spawner,
                    };
                    let options = PeerConnectionOptions {
                        connect_timeout: state.options.peer_connect_timeout,
                        ..Default::default()
                    };
                    let peer_connection = PeerConnection::new(
                        addr,
                        state.info_hash,
                        state.peer_id,
                        handler,
                        Some(options),
                        spawner,
                    );
                    spawn(format!("manage_peer({})", addr), async move {
                        if let Err(e) = peer_connection.manage_peer(out_rx).await {
                            debug!("error managing peer {}: {:#}", addr, e)
                        };
                        let state = peer_connection.into_handler().state;
                        state.drop_peer(addr);
                        state.peer_semaphore.add_permits(1);
                        Ok::<_, anyhow::Error>(())
                    });
                }
            }
        });
        state
    }
    pub fn info(&self) -> &TorrentMetaV1Info<ByteString> {
        &self.info
    }
    pub fn info_hash(&self) -> Id20 {
        self.info_hash
    }
    pub fn peer_id(&self) -> Id20 {
        self.peer_id
    }
    pub fn file_ops(&self) -> FileOps<'_, Sha1> {
        FileOps::new(&self.info, &self.files, &self.lengths)
    }
    pub fn initially_needed(&self) -> u64 {
        self.needed
    }
    pub fn lock_read(&self) -> RwLockReadGuard<TorrentStateLocked> {
        self.locked.read()
    }

    fn get_next_needed_piece(&self, peer_handle: PeerHandle) -> Option<ValidPieceIndex> {
        let g = self.locked.read();
        let bf = g.peers.get_live(peer_handle)?.bitfield.as_ref()?;
        for n in g.chunks.iter_needed_pieces() {
            if bf.get(n).map(|v| *v) == Some(true) {
                // in theory it should be safe without validation, but whatever.
                return self.lengths.validate_piece_index(n as u32);
            }
        }
        None
    }

    fn am_i_choked(&self, peer_handle: PeerHandle) -> Option<bool> {
        self.locked
            .read()
            .peers
            .get_live(peer_handle)
            .map(|l| l.i_am_choked)
    }

    fn reserve_next_needed_piece(&self, peer_handle: PeerHandle) -> Option<ValidPieceIndex> {
        if self.am_i_choked(peer_handle)? {
            warn!("we are choked by {}, can't reserve next piece", peer_handle);
            return None;
        }
        let mut g = self.locked.write();
        let n = {
            let mut n_opt = None;
            let bf = g.peers.get_live(peer_handle)?.bitfield.as_ref()?;
            for n in g.chunks.iter_needed_pieces() {
                if bf.get(n).map(|v| *v) == Some(true) {
                    n_opt = Some(n);
                    break;
                }
            }

            self.lengths.validate_piece_index(n_opt? as u32)?
        };
        g.peers.inflight_pieces.insert(
            n,
            InflightPiece {
                peer: peer_handle,
                started: Instant::now(),
            },
        );
        g.chunks.reserve_needed_piece(n);
        Some(n)
    }

    fn am_i_interested_in_peer(&self, handle: PeerHandle) -> bool {
        self.get_next_needed_piece(handle).is_some()
    }

    fn try_steal_old_slow_piece(&self, handle: PeerHandle) -> Option<ValidPieceIndex> {
        let total = self.stats.downloaded_pieces.load(Ordering::Relaxed);

        // heuristic for not enough precision in average time
        if total < 20 {
            return None;
        }
        let avg_time = self.stats.average_piece_download_time()?;

        let mut g = self.locked.write();
        let (idx, elapsed, piece_req) = g
            .peers
            .inflight_pieces
            .iter_mut()
            // don't steal from myself
            .filter(|(_, r)| r.peer != handle)
            .map(|(p, r)| (p, r.started.elapsed(), r))
            .max_by_key(|(_, e, _)| *e)?;

        // heuristic for "too slow peer"
        if elapsed > avg_time * 10 {
            debug!(
                "{} will steal piece {} from {}: elapsed time {:?}, avg piece time: {:?}",
                handle, idx, piece_req.peer, elapsed, avg_time
            );
            piece_req.peer = handle;
            piece_req.started = Instant::now();
            return Some(*idx);
        }
        None
    }

    fn try_steal_piece(&self, handle: PeerHandle) -> Option<ValidPieceIndex> {
        let mut rng = rand::thread_rng();
        use rand::seq::IteratorRandom;
        let g = self.locked.read();
        let pl = g.peers.get_live(handle)?;
        g.peers
            .inflight_pieces
            .keys()
            .filter(|p| !pl.inflight_requests.iter().any(|req| req.piece == **p))
            .choose(&mut rng)
            .copied()
    }

    fn set_peer_live(&self, handle: PeerHandle, h: Handshake) {
        let mut g = self.locked.write();
        match g.peers.states.get_mut(&handle) {
            Some(s @ &mut PeerState::Connecting) => {
                *s = PeerState::Live(LivePeerState::new(Id20(h.peer_id)));
            }
            _ => {
                warn!("peer {} was in wrong state", handle);
            }
        }
    }

    fn drop_peer(&self, handle: PeerHandle) -> bool {
        let mut g = self.locked.write();
        let peer = match g.peers.drop_peer(handle) {
            Some(peer) => peer,
            None => return false,
        };
        if let PeerState::Live(l) = peer {
            for req in l.inflight_requests {
                g.chunks.mark_chunk_request_cancelled(req.piece, req.chunk);
            }
        }
        true
    }

    pub fn get_uploaded(&self) -> u64 {
        self.stats.uploaded.load(Ordering::Relaxed)
    }
    pub fn get_downloaded(&self) -> u64 {
        self.stats.downloaded_and_checked.load(Ordering::Relaxed)
    }

    pub fn get_left_to_download(&self) -> u64 {
        self.needed - self.get_downloaded()
    }

    fn maybe_transmit_haves(&self, index: ValidPieceIndex) {
        let mut futures = Vec::new();

        let g = self.locked.read();
        for (handle, peer_state) in g.peers.states.iter() {
            match peer_state {
                PeerState::Live(live) => {
                    if !live.peer_interested {
                        continue;
                    }

                    if live
                        .bitfield
                        .as_ref()
                        .and_then(|b| b.get(index.get() as usize).map(|v| *v))
                        .unwrap_or(false)
                    {
                        continue;
                    }

                    let tx = match g.peers.tx.get(handle) {
                        Some(tx) => tx,
                        None => continue,
                    };
                    let tx = Arc::downgrade(tx);
                    futures.push(async move {
                        if let Some(tx) = tx.upgrade() {
                            if tx
                                .send(WriterRequest::Message(Message::Have(index.get())))
                                .is_err()
                            {
                                // whatever
                            }
                        }
                    });
                }
                _ => continue,
            }
        }

        if futures.is_empty() {
            trace!("no peers to transmit Have={} to, saving some work", index);
            return;
        }

        let mut unordered: FuturesUnordered<_> = futures.into_iter().collect();
        spawn(
            format!("transmit_haves(piece={}, count={})", index, unordered.len()),
            async move {
                while unordered.next().await.is_some() {}
                Ok(())
            },
        );
    }

    pub fn add_peer_if_not_seen(self: &Arc<Self>, addr: SocketAddr) -> bool {
        let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel::<WriterRequest>();
        match self.locked.write().peers.add_if_not_seen(addr, out_tx) {
            Some(handle) => handle,
            None => return false,
        };

        match self.peer_queue_tx.send((addr, out_rx)) {
            Ok(_) => {}
            Err(_) => {
                warn!("peer adder died, can't add peer")
            }
        }
        true
    }

    pub fn peer_stats_snapshot(&self) -> AggregatePeerStats {
        self.locked.read().peers.stats()
    }

    pub fn stats_snapshot(&self) -> StatsSnapshot {
        let g = self.locked.read();
        use Ordering::*;
        let peer_stats = g.peers.stats();
        let downloaded = self.stats.downloaded_and_checked.load(Relaxed);
        let remaining = self.needed - downloaded;
        StatsSnapshot {
            have_bytes: self.stats.have.load(Relaxed),
            downloaded_and_checked_bytes: downloaded,
            downloaded_and_checked_pieces: self.stats.downloaded_pieces.load(Relaxed),
            fetched_bytes: self.stats.fetched_bytes.load(Relaxed),
            uploaded_bytes: self.stats.uploaded.load(Relaxed),
            total_bytes: self.have_plus_needed,
            live_peers: peer_stats.live as u32,
            seen_peers: g.peers.seen.len() as u32,
            connecting_peers: peer_stats.connecting as u32,
            time: Instant::now(),
            initially_needed_bytes: self.needed,
            remaining_bytes: remaining,
            queued_peers: peer_stats.queued as u32,
            total_piece_download_ms: self.stats.total_piece_download_ms.load(Relaxed),
        }
    }
}

#[derive(Clone)]
struct PeerHandler {
    state: Arc<TorrentState>,
    addr: SocketAddr,
    spawner: BlockingSpawner,
}

impl PeerConnectionHandler for PeerHandler {
    fn on_received_message(&self, message: Message<ByteBuf<'_>>) -> anyhow::Result<()> {
        match message {
            Message::Request(request) => {
                self.on_download_request(self.addr, request)
                    .with_context(|| {
                        format!("error handling download request from {}", self.addr)
                    })?;
            }
            Message::Bitfield(b) => self.on_bitfield(self.addr, b.clone_to_owned())?,
            Message::Choke => self.on_i_am_choked(self.addr),
            Message::Unchoke => self.on_i_am_unchoked(self.addr),
            Message::Interested => self.on_peer_interested(self.addr),
            Message::Piece(piece) => {
                self.on_received_piece(self.addr, piece)
                    .context("error in on_received_piece()")?;
            }
            Message::KeepAlive => {
                debug!("keepalive received from {}", self.addr);
            }
            Message::Have(h) => self.on_have(self.addr, h),
            Message::NotInterested => {
                info!("received \"not interested\", but we don't care yet")
            }
            message => {
                warn!(
                    "{}: received unsupported message {:?}, ignoring",
                    self.addr, message
                );
            }
        }
        Ok(())
    }

    fn get_have_bytes(&self) -> u64 {
        self.state.stats.have.load(Ordering::Relaxed)
    }

    fn serialize_bitfield_message_to_buf(&self, buf: &mut Vec<u8>) -> Option<usize> {
        let g = self.state.locked.read();
        let msg = Message::Bitfield(ByteBuf(g.chunks.get_have_pieces().as_raw_slice()));
        let len = msg.serialize(buf, None).unwrap();
        debug!("sending to {}: {:?}, length={}", self.addr, &msg, len);
        Some(len)
    }

    fn on_handshake(&self, handshake: Handshake) -> anyhow::Result<()> {
        self.state.set_peer_live(self.addr, handshake);
        Ok(())
    }

    fn on_uploaded_bytes(&self, bytes: u32) {
        self.state
            .stats
            .uploaded
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    fn read_chunk(&self, chunk: &ChunkInfo, buf: &mut [u8]) -> anyhow::Result<()> {
        self.state.file_ops().read_chunk(self.addr, chunk, buf)
    }

    fn on_extended_handshake(&self, _: &ExtendedHandshake<ByteBuf>) -> anyhow::Result<()> {
        Ok(())
    }
}

impl PeerHandler {
    fn on_download_request(&self, peer_handle: PeerHandle, request: Request) -> anyhow::Result<()> {
        let piece_index = match self.state.lengths.validate_piece_index(request.index) {
            Some(p) => p,
            None => {
                anyhow::bail!(
                    "{}: received {:?}, but it is not a valid chunk request (piece index is invalid). Ignoring.",
                    peer_handle, request
                );
            }
        };
        let chunk_info = match self.state.lengths.chunk_info_from_received_data(
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

        let tx = {
            let g = self.state.locked.read();
            if !g.chunks.is_chunk_ready_to_upload(&chunk_info) {
                anyhow::bail!(
                    "got request for a chunk that is not ready to upload. chunk {:?}",
                    &chunk_info
                );
            }

            g.peers.clone_tx(peer_handle).ok_or_else(|| {
                anyhow::anyhow!(
                    "peer {} died, dropping chunk that it requested",
                    peer_handle
                )
            })?
        };

        // TODO: this is not super efficient as it does copying multiple times.
        // Theoretically, this could be done in the sending code, so that it reads straight into
        // the send buffer.
        let request = WriterRequest::ReadChunkRequest(chunk_info);
        debug!("sending to {}: {:?}", peer_handle, &request);
        Ok::<_, anyhow::Error>(tx.send(request)?)
    }

    fn on_have(&self, handle: PeerHandle, have: u32) {
        if let Some(bitfield) = self
            .state
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

    fn on_bitfield(&self, handle: PeerHandle, bitfield: ByteString) -> anyhow::Result<()> {
        if bitfield.len() != self.state.lengths.piece_bitfield_bytes() as usize {
            anyhow::bail!(
                "dropping {} as its bitfield has unexpected size. Got {}, expected {}",
                handle,
                bitfield.len(),
                self.state.lengths.piece_bitfield_bytes(),
            );
        }
        self.state
            .locked
            .write()
            .peers
            .update_bitfield_from_vec(handle, bitfield.0);

        if !self.state.am_i_interested_in_peer(handle) {
            let tx = self
                .state
                .locked
                .read()
                .peers
                .clone_tx(handle)
                .ok_or_else(|| anyhow::anyhow!("peer closed"))?;
            tx.send(WriterRequest::Message(MessageOwned::Unchoke))
                .context("peer dropped")?;
            tx.send(WriterRequest::Message(MessageOwned::NotInterested))
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
        let tx = match self.state.locked.read().peers.clone_tx(handle) {
            Some(tx) => tx,
            None => return Ok(()),
        };
        tx.send(WriterRequest::Message(MessageOwned::Unchoke))
            .context("peer dropped")?;
        tx.send(WriterRequest::Message(MessageOwned::Interested))
            .context("peer dropped")?;

        self.requester(handle).await?;
        Ok::<_, anyhow::Error>(())
    }

    fn on_i_am_choked(&self, handle: PeerHandle) {
        warn!("we are choked by {}", handle);
        self.state
            .locked
            .write()
            .peers
            .mark_i_am_choked(handle, true);
    }

    fn on_peer_interested(&self, handle: PeerHandle) {
        debug!("peer {} is interested", handle);
        self.state
            .locked
            .write()
            .peers
            .mark_peer_interested(handle, true);
    }

    async fn requester(self, handle: PeerHandle) -> anyhow::Result<()> {
        let notify = match self.state.locked.read().peers.get_live(handle) {
            Some(l) => l.have_notify.clone(),
            None => return Ok(()),
        };

        // TODO: this might dangle, same below.
        #[allow(unused_must_use)]
        {
            timeout(Duration::from_secs(60), notify.notified()).await;
        }

        loop {
            match self.state.am_i_choked(handle) {
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

            let next = match self.state.try_steal_old_slow_piece(handle) {
                Some(next) => next,
                None => match self.state.reserve_next_needed_piece(handle) {
                    Some(next) => next,
                    None => {
                        if self.state.get_left_to_download() == 0 {
                            debug!("{}: nothing left to download, closing requester", handle);
                            return Ok(());
                        }

                        if let Some(piece) = self.state.try_steal_piece(handle) {
                            debug!("{}: stole a piece {}", handle, piece);
                            piece
                        } else {
                            debug!("no pieces to request from {}", handle);
                            #[allow(unused_must_use)]
                            {
                                timeout(Duration::from_secs(60), notify.notified()).await;
                            }
                            continue;
                        }
                    }
                },
            };

            let tx = match self.state.locked.read().peers.clone_tx(handle) {
                Some(tx) => tx,
                None => return Ok(()),
            };
            let sem = match self.state.locked.read().peers.get_live(handle) {
                Some(live) => live.requests_sem.clone(),
                None => return Ok(()),
            };
            for chunk in self.state.lengths.iter_chunk_infos(next) {
                if self.state.locked.read().chunks.is_chunk_downloaded(&chunk) {
                    continue;
                }
                if !self
                    .state
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

                tx.send(WriterRequest::Message(MessageOwned::Request(request)))
                    .context("peer dropped")?;
            }
        }
    }

    fn on_i_am_unchoked(&self, handle: PeerHandle) {
        debug!("we are unchoked by {}", handle);
        let mut g = self.state.locked.write();
        let live = match g.peers.get_live_mut(handle) {
            Some(live) => live,
            None => return,
        };
        live.i_am_choked = false;
        live.have_notify.notify_waiters();
        live.requests_sem.add_permits(16);
    }

    fn on_received_piece(&self, handle: PeerHandle, piece: Piece<ByteBuf>) -> anyhow::Result<()> {
        let chunk_info = match self.state.lengths.chunk_info_from_received_piece(
            piece.index,
            piece.begin,
            piece.block.len() as u32,
        ) {
            Some(i) => i,
            None => {
                anyhow::bail!(
                    "peer {} sent us a piece that is invalid {:?}",
                    handle,
                    &piece,
                );
            }
        };

        let mut g = self.state.locked.write();
        let h = g.peers.try_get_live_mut(handle)?;
        h.requests_sem.add_permits(1);

        self.state
            .stats
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

        let full_piece_download_time = match g.chunks.mark_chunk_downloaded(&piece) {
            Some(ChunkMarkingResult::Completed) => {
                debug!(
                    "piece={} done by {}, will write and checksum",
                    piece.index, handle
                );
                // This will prevent others from stealing it.
                g.peers
                    .remove_inflight_piece(chunk_info.piece_index)
                    .map(|t| t.started.elapsed())
            }
            Some(ChunkMarkingResult::PreviouslyCompleted) => {
                // TODO: we might need to send cancellations here.
                debug!(
                    "piece={} was done by someone else {}, ignoring",
                    piece.index, handle
                );
                return Ok(());
            }
            Some(ChunkMarkingResult::NotCompleted) => None,
            None => {
                anyhow::bail!(
                    "bogus data received from {}: {:?}, cannot map this to a chunk, dropping peer",
                    handle,
                    piece
                );
            }
        };

        // to prevent deadlocks.
        drop(g);

        self.spawner
            .spawn_block_in_place(move || {
                let index = piece.index;

                // TODO: in theory we should unmark the piece as downloaded here. But if there was a disk error, what
                // should we really do? If we unmark it, it will get requested forever...
                //
                // So let's just unwrap and abort.
                self.state
                    .file_ops()
                    .write_chunk(handle, &piece, &chunk_info)
                    .expect("expected to be able to write to disk");

                let full_piece_download_time = match full_piece_download_time {
                    Some(t) => t,
                    None => return Ok(()),
                };

                match self
                    .state
                    .file_ops()
                    .check_piece(handle, chunk_info.piece_index, &chunk_info)
                    .with_context(|| format!("error checking piece={}", index))?
                {
                    true => {
                        let piece_len =
                            self.state.lengths.piece_length(chunk_info.piece_index) as u64;
                        self.state
                            .stats
                            .downloaded_and_checked
                            .fetch_add(piece_len, Ordering::Relaxed);
                        self.state
                            .stats
                            .have
                            .fetch_add(piece_len, Ordering::Relaxed);
                        self.state
                            .stats
                            .downloaded_pieces
                            .fetch_add(1, Ordering::Relaxed);
                        self.state
                            .stats
                            .downloaded_pieces
                            .fetch_add(1, Ordering::Relaxed);
                        self.state.stats.total_piece_download_ms.fetch_add(
                            full_piece_download_time.as_millis() as u64,
                            Ordering::Relaxed,
                        );
                        self.state
                            .locked
                            .write()
                            .chunks
                            .mark_piece_downloaded(chunk_info.piece_index);

                        debug!(
                            "piece={} successfully downloaded and verified from {}",
                            index, handle
                        );

                        self.state.maybe_transmit_haves(chunk_info.piece_index);
                    }
                    false => {
                        warn!(
                            "checksum for piece={} did not validate, came from {}",
                            index, handle
                        );
                        self.state
                            .locked
                            .write()
                            .chunks
                            .mark_piece_broken(chunk_info.piece_index);
                    }
                };
                Ok::<_, anyhow::Error>(())
            })
            .with_context(|| format!("error processing received chunk {:?}", chunk_info))?;
        Ok(())
    }
}
