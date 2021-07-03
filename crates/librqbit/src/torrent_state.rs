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
use futures::{stream::FuturesUnordered, StreamExt};
use log::{debug, info, trace, warn};
use parking_lot::{Mutex, RwLock};
use tokio::{sync::mpsc::UnboundedSender, time::timeout};

use crate::{
    buffers::{ByteBuf, ByteString},
    chunk_tracker::{ChunkMarkingResult, ChunkTracker},
    clone_to_owned::CloneToOwned,
    file_ops::FileOps,
    lengths::{Lengths, ValidPieceIndex},
    peer_binary_protocol::{
        extended::handshake::ExtendedHandshake, Handshake, Message, MessageOwned, Piece, Request,
    },
    peer_connection::{PeerConnection, PeerConnectionHandler, WriterRequest},
    peer_state::{InflightRequest, LivePeerState, PeerState},
    spawn_utils::{spawn, BlockingSpawner},
    torrent_metainfo::TorrentMetaV1Info,
    type_aliases::{PeerHandle, Sha1, BF},
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
    pub connecting: usize,
    pub live: usize,
}

impl PeerStates {
    pub fn stats(&self) -> AggregatePeerStats {
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
        self.states.insert(handle, PeerState::Connecting(addr));
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
pub struct AtomicStats {
    pub have: AtomicU64,
    pub downloaded_and_checked: AtomicU64,
    pub uploaded: AtomicU64,
    pub fetched_bytes: AtomicU64,

    pub downloaded_pieces: AtomicU64,
    pub total_piece_download_ms: AtomicU64,
}

impl AtomicStats {
    pub fn average_piece_download_time(&self) -> Option<Duration> {
        let d = self.downloaded_pieces.load(Ordering::Relaxed);
        let t = self.total_piece_download_ms.load(Ordering::Relaxed);
        if d == 0 {
            return None;
        }
        Some(Duration::from_secs_f64(t as f64 / d as f64 / 1000f64))
    }
}

#[derive(Debug)]
pub struct StatsSnapshot {
    pub have_bytes: u64,
    pub downloaded_and_checked_bytes: u64,
    pub downloaded_and_checked_pieces: u64,
    pub fetched_bytes: u64,
    pub uploaded_bytes: u64,
    pub initially_needed_bytes: u64,
    pub remaining_bytes: u64,
    pub live_peers: u32,
    pub seen_peers: u32,
    pub connecting_peers: u32,
    pub time: Instant,
}

pub struct TorrentState {
    pub torrent: TorrentMetaV1Info<ByteString>,
    pub locked: Arc<RwLock<TorrentStateLocked>>,
    pub files: Vec<Arc<Mutex<File>>>,
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub lengths: Lengths,
    pub needed: u64,
    pub stats: AtomicStats,

    pub spawner: BlockingSpawner,
}

impl TorrentState {
    pub fn file_ops(&self) -> FileOps<'_, Sha1> {
        FileOps::new(&self.torrent, &self.files, &self.lengths)
    }

    pub fn get_next_needed_piece(&self, peer_handle: PeerHandle) -> Option<ValidPieceIndex> {
        let g = self.locked.read();
        let bf = g.peers.get_live(peer_handle)?.bitfield.as_ref()?;
        for n in g.chunks.get_needed_pieces().iter_ones() {
            if bf.get(n).map(|v| *v) == Some(true) {
                // in theory it should be safe without validation, but whatever.
                return self.lengths.validate_piece_index(n as u32);
            }
        }
        None
    }

    pub fn am_i_choked(&self, peer_handle: PeerHandle) -> Option<bool> {
        self.locked
            .read()
            .peers
            .get_live(peer_handle)
            .map(|l| l.i_am_choked)
    }

    pub fn reserve_next_needed_piece(&self, peer_handle: PeerHandle) -> Option<ValidPieceIndex> {
        if self.am_i_choked(peer_handle)? {
            warn!("we are choked by {}, can't reserve next piece", peer_handle);
            return None;
        }
        let mut g = self.locked.write();
        let n = {
            let mut n_opt = None;
            let bf = g.peers.get_live(peer_handle)?.bitfield.as_ref()?;
            for n in g.chunks.get_needed_pieces().iter_ones() {
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

    pub fn am_i_interested_in_peer(&self, handle: PeerHandle) -> bool {
        self.get_next_needed_piece(handle).is_some()
    }

    pub fn try_steal_old_slow_piece(&self, handle: PeerHandle) -> Option<ValidPieceIndex> {
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

    pub fn try_steal_piece(&self, handle: PeerHandle) -> Option<ValidPieceIndex> {
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

    pub fn set_peer_live(&self, handle: PeerHandle, h: Handshake) {
        let mut g = self.locked.write();
        match g.peers.states.get_mut(&handle) {
            Some(s @ &mut PeerState::Connecting(_)) => {
                *s = PeerState::Live(LivePeerState::new(h.peer_id));
            }
            _ => {
                warn!("peer {} was in wrong state", handle);
            }
        }
    }

    pub fn drop_peer(&self, handle: PeerHandle) -> bool {
        let mut g = self.locked.write();
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

    pub fn get_uploaded(&self) -> u64 {
        self.stats.uploaded.load(Ordering::Relaxed)
    }
    pub fn get_downloaded(&self) -> u64 {
        self.stats.downloaded_and_checked.load(Ordering::Relaxed)
    }

    pub fn get_left_to_download(&self) -> u64 {
        self.needed - self.get_downloaded()
    }

    pub fn maybe_transmit_haves(&self, index: ValidPieceIndex) {
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
        let handle = match self.locked.write().peers.add_if_not_seen(addr, out_tx) {
            Some(handle) => handle,
            None => return false,
        };

        let handler = PeerHandler {
            addr,
            state: self.clone(),
            spawner: self.spawner,
        };
        let peer_connection = PeerConnection::new(addr, self.info_hash, self.peer_id, handler);
        spawn(format!("manage_peer({})", handle), async move {
            if let Err(e) = peer_connection.manage_peer(out_rx).await {
                debug!("error managing peer {}: {:#}", handle, e)
            };
            peer_connection.into_handler().state.drop_peer(handle);
            Ok::<_, anyhow::Error>(())
        });
        true
    }

    pub fn stats_snapshot(&self) -> StatsSnapshot {
        let g = self.locked.read();
        use Ordering::*;
        let (live, connecting) =
            g.peers
                .states
                .values()
                .fold((0u32, 0u32), |(live, connecting), p| match p {
                    PeerState::Connecting(_) => (live, connecting + 1),
                    PeerState::Live(_) => (live + 1, connecting),
                });
        let downloaded = self.stats.downloaded_and_checked.load(Relaxed);
        let remaining = self.needed - downloaded;
        StatsSnapshot {
            have_bytes: self.stats.have.load(Relaxed),
            downloaded_and_checked_bytes: downloaded,
            downloaded_and_checked_pieces: self.stats.downloaded_pieces.load(Relaxed),
            fetched_bytes: self.stats.fetched_bytes.load(Relaxed),
            uploaded_bytes: self.stats.fetched_bytes.load(Relaxed),
            live_peers: live,
            seen_peers: g.peers.seen.len() as u32,
            connecting_peers: connecting,
            time: Instant::now(),
            initially_needed_bytes: self.needed,
            remaining_bytes: remaining,
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

    fn read_chunk(&self, chunk: &crate::lengths::ChunkInfo, buf: &mut [u8]) -> anyhow::Result<()> {
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
        let chunk_info = match self.state.lengths.chunk_info_from_received_piece(&piece) {
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
