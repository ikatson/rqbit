// The main logic of rqbit is here - connecting to peers, reading and writing messages
// to them, tracking peer state etc.
//
// ## Architecture
// There are many tasks cooperating to download the torrent. Tasks communicate both with message passing
// and shared memory.
//
// ### Shared locked state
// Shared state is access by almost all actors through RwLocks.
//
// There's one source of truth (TorrentStateLocked) for which chunks we have, need, and what peers are we waiting them from.
//
// Peer states that are important to the outsiders (tasks other than manage_peer) are in a sharded hash-map (DashMap)
//
// ### Tasks (actors)
// Peer adder task:
// - spawns new peers as they become known. It pulls them from a queue. The queue is filled in by DHT and torrent trackers.
//   Also gets updated when peers are reconnecting after errors.
//
// Each peer has one main task "manage_peer". It's composed of 2 futures running as one task through tokio::select:
// - "manage_peer" - this talks to the peer over network and calls callbacks on PeerHandler. The callbacks are not async,
//   and are supposed to finish quickly (apart from writing to disk, which is accounted for as "spawn_blocking").
// - "peer_chunk_requester" - this continuously sends requests for chunks to the peer.
//   it may steal chunks/pieces from other peers.
//
// ## Peer lifecycle
// State transitions:
// - queued (initial state) -> connected
// - connected -> live
// - ANY STATE -> dead (on error)
// - ANY STATE -> not_needed (when we don't need to talk to the peer anymore)
//
// When the peer dies, it's rescheduled with exponential backoff.
//
// > NOTE: deadlock notice:
// > peers and stateLocked are behind 2 different locks.
// > if you lock them in different order, this may deadlock.
// >
// > so don't lock them both at the same time at all, or at the worst lock them in the
// > same order (peers one first, then the global one).

pub mod peer;
pub mod peers;
pub mod stats;

use std::{
    collections::HashMap,
    fs::File,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{bail, Context};
use backoff::backoff::Backoff;
use buffers::{ByteBuf, ByteString};
use clone_to_owned::CloneToOwned;
use futures::{stream::FuturesUnordered, StreamExt};
use itertools::Itertools;
use librqbit_core::{
    hash_id::Id20,
    lengths::{ChunkInfo, Lengths, ValidPieceIndex},
    spawn_utils::spawn_with_cancel,
    speed_estimator::SpeedEstimator,
    torrent_metainfo::TorrentMetaV1Info,
};
use parking_lot::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use peer_binary_protocol::{
    extended::handshake::ExtendedHandshake, Handshake, Message, MessageOwned, Piece, Request,
};
use sha1w::Sha1;
use tokio::{
    sync::{
        mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
        Notify, OwnedSemaphorePermit, Semaphore,
    },
    time::timeout,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, error_span, info, trace, warn};

use crate::{
    chunk_tracker::{ChunkMarkingResult, ChunkTracker},
    file_ops::FileOps,
    peer_connection::{
        PeerConnection, PeerConnectionHandler, PeerConnectionOptions, WriterRequest,
    },
    session::CheckedIncomingConnection,
    torrent_state::{peer::Peer, utils::atomic_inc},
    type_aliases::{PeerHandle, BF},
};

use self::{
    peer::{
        stats::{
            atomic::PeerCountersAtomic as AtomicPeerCounters,
            snapshot::{PeerStatsFilter, PeerStatsSnapshot},
        },
        InflightRequest, PeerRx, PeerState, PeerTx,
    },
    peers::PeerStates,
    stats::{atomic::AtomicStats, snapshot::StatsSnapshot},
};

use super::{
    paused::TorrentStatePaused,
    utils::{timeit, TimedExistence},
    ManagedTorrentInfo,
};

struct InflightPiece {
    peer: PeerHandle,
    started: Instant,
}

fn dummy_file() -> anyhow::Result<std::fs::File> {
    #[cfg(target_os = "windows")]
    const DEVNULL: &str = "NUL";
    #[cfg(not(target_os = "windows"))]
    const DEVNULL: &str = "/dev/null";

    std::fs::OpenOptions::new()
        .read(true)
        .open(DEVNULL)
        .with_context(|| format!("error opening {}", DEVNULL))
}

fn make_piece_bitfield(lengths: &Lengths) -> BF {
    BF::from_vec(vec![0; lengths.piece_bitfield_bytes()])
}

pub(crate) struct TorrentStateLocked {
    // What chunks we have and need.
    // If this is None, the torrent was paused, and this live state is useless, and needs to be dropped.
    pub(crate) chunks: Option<ChunkTracker>,

    // At a moment in time, we are expecting a piece from only one peer.
    // inflight_pieces stores this information.
    inflight_pieces: HashMap<ValidPieceIndex, InflightPiece>,

    // If this is None, then it was already used
    fatal_errors_tx: Option<tokio::sync::oneshot::Sender<anyhow::Error>>,
}

impl TorrentStateLocked {
    pub(crate) fn get_chunks(&self) -> anyhow::Result<&ChunkTracker> {
        self.chunks
            .as_ref()
            .context("chunk tracker empty, torrent was paused")
    }

    fn get_chunks_mut(&mut self) -> anyhow::Result<&mut ChunkTracker> {
        self.chunks
            .as_mut()
            .context("chunk tracker empty, torrent was paused")
    }
}

#[derive(Default)]
pub struct TorrentStateOptions {
    pub peer_connect_timeout: Option<Duration>,
    pub peer_read_write_timeout: Option<Duration>,
}

pub struct TorrentStateLive {
    peers: PeerStates,
    meta: Arc<ManagedTorrentInfo>,
    locked: RwLock<TorrentStateLocked>,

    files: Vec<Arc<Mutex<File>>>,
    filenames: Vec<PathBuf>,

    initially_needed_bytes: u64,
    total_selected_bytes: u64,

    stats: AtomicStats,
    lengths: Lengths,

    // Limits how many active (occupying network resources) peers there are at a moment in time.
    peer_semaphore: Arc<Semaphore>,

    // The queue for peer manager to connect to them.
    peer_queue_tx: UnboundedSender<SocketAddr>,

    finished_notify: Notify,

    down_speed_estimator: SpeedEstimator,
    up_speed_estimator: SpeedEstimator,
    cancellation_token: CancellationToken,
}

impl TorrentStateLive {
    pub(crate) fn new(
        paused: TorrentStatePaused,
        fatal_errors_tx: tokio::sync::oneshot::Sender<anyhow::Error>,
        cancellation_token: CancellationToken,
    ) -> Arc<Self> {
        let (peer_queue_tx, peer_queue_rx) = unbounded_channel();

        let down_speed_estimator = SpeedEstimator::new(5);
        let up_speed_estimator = SpeedEstimator::new(5);

        let have_bytes = paused.have_bytes;
        let needed_bytes = paused.needed_bytes;
        let total_selected_bytes = paused.chunk_tracker.get_total_selected_bytes();
        let lengths = *paused.chunk_tracker.get_lengths();

        let state = Arc::new(TorrentStateLive {
            meta: paused.info.clone(),
            peers: Default::default(),
            locked: RwLock::new(TorrentStateLocked {
                chunks: Some(paused.chunk_tracker),
                inflight_pieces: Default::default(),
                fatal_errors_tx: Some(fatal_errors_tx),
            }),
            files: paused.files,
            filenames: paused.filenames,
            stats: AtomicStats {
                have_bytes: AtomicU64::new(have_bytes),
                ..Default::default()
            },
            initially_needed_bytes: needed_bytes,
            lengths,
            total_selected_bytes,
            peer_semaphore: Arc::new(Semaphore::new(128)),
            peer_queue_tx,
            finished_notify: Notify::new(),
            down_speed_estimator,
            up_speed_estimator,
            cancellation_token,
        });

        state.spawn(
            error_span!(parent: state.meta.span.clone(), "speed_estimator_updater"),
            {
                let state = Arc::downgrade(&state);
                async move {
                    loop {
                        let state = match state.upgrade() {
                            Some(state) => state,
                            None => return Ok(()),
                        };
                        let now = Instant::now();
                        let stats = state.stats_snapshot();
                        let fetched = stats.fetched_bytes;
                        let needed = state.initially_needed();
                        // fetched can be too high in theory, so for safety make sure that it doesn't wrap around u64.
                        let remaining = needed
                            .wrapping_sub(fetched)
                            .min(needed - stats.downloaded_and_checked_bytes);
                        state
                            .down_speed_estimator
                            .add_snapshot(fetched, Some(remaining), now);
                        state
                            .up_speed_estimator
                            .add_snapshot(stats.uploaded_bytes, None, now);
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            },
        );

        state.spawn(
            error_span!(parent: state.meta.span.clone(), "peer_adder"),
            state.clone().task_peer_adder(peer_queue_rx),
        );
        state
    }

    pub(crate) fn spawn(
        &self,
        span: tracing::Span,
        fut: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    ) {
        spawn_with_cancel(span, self.cancellation_token.clone(), fut);
    }

    pub fn down_speed_estimator(&self) -> &SpeedEstimator {
        &self.down_speed_estimator
    }

    pub fn up_speed_estimator(&self) -> &SpeedEstimator {
        &self.up_speed_estimator
    }

    pub(crate) fn add_incoming_peer(
        self: &Arc<Self>,
        checked_peer: CheckedIncomingConnection,
    ) -> anyhow::Result<()> {
        use dashmap::mapref::entry::Entry;
        let (tx, rx) = unbounded_channel();
        let permit = match self.peer_semaphore.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                warn!("limit of live peers reached, dropping incoming peer");
                self.peers.with_peer(checked_peer.addr, |p| {
                    atomic_inc(&p.stats.counters.incoming_connections);
                });
                return Ok(());
            }
        };

        let counters = match self.peers.states.entry(checked_peer.addr) {
            Entry::Occupied(mut occ) => {
                let peer = occ.get_mut();
                peer.state
                    .incoming_connection(
                        Id20::new(checked_peer.handshake.peer_id),
                        tx.clone(),
                        &self.peers.stats,
                    )
                    .context("peer already existed")?;
                peer.stats.counters.clone()
            }
            Entry::Vacant(vac) => {
                atomic_inc(&self.peers.stats.seen);
                let peer = Peer::new_live_for_incoming_connection(
                    Id20::new(checked_peer.handshake.peer_id),
                    tx.clone(),
                    &self.peers.stats,
                );
                let counters = peer.stats.counters.clone();
                vac.insert(peer);
                counters
            }
        };
        atomic_inc(&counters.incoming_connections);

        self.spawn(
            error_span!(
                parent: self.meta.span.clone(),
                "manage_incoming_peer",
                addr = %checked_peer.addr
            ),
            self.clone()
                .task_manage_incoming_peer(checked_peer, counters, tx, rx, permit),
        );
        Ok(())
    }

    async fn task_manage_incoming_peer(
        self: Arc<Self>,
        checked_peer: CheckedIncomingConnection,
        counters: Arc<AtomicPeerCounters>,
        tx: PeerTx,
        rx: PeerRx,
        permit: OwnedSemaphorePermit,
    ) -> anyhow::Result<()> {
        // TODO: bump counters for incoming
        let handler = PeerHandler {
            addr: checked_peer.addr,
            on_bitfield_notify: Default::default(),
            unchoke_notify: Default::default(),
            locked: RwLock::new(PeerHandlerLocked { i_am_choked: true }),
            requests_sem: Semaphore::new(0),
            state: self.clone(),
            tx,
            counters,
        };
        let options = PeerConnectionOptions {
            connect_timeout: self.meta.options.peer_connect_timeout,
            read_write_timeout: self.meta.options.peer_read_write_timeout,
            ..Default::default()
        };
        let peer_connection = PeerConnection::new(
            checked_peer.addr,
            self.meta.info_hash,
            self.meta.peer_id,
            &handler,
            Some(options),
            self.meta.spawner,
        );
        let requester = handler.task_peer_chunk_requester();

        let res = tokio::select! {
            r = requester => {r}
            r = peer_connection.manage_peer_incoming(
                rx,
                checked_peer.read_buf,
                checked_peer.handshake,
                checked_peer.stream
            ) => {r}
        };

        match res {
            // We disconnected the peer ourselves as we don't need it
            Ok(()) => {
                handler.on_peer_died(None)?;
            }
            Err(e) => {
                debug!("error managing peer: {:#}", e);
                handler.on_peer_died(Some(e))?;
            }
        };
        drop(permit);
        Ok(())
    }

    async fn task_manage_outgoing_peer(
        self: Arc<Self>,
        addr: SocketAddr,
        permit: OwnedSemaphorePermit,
    ) -> anyhow::Result<()> {
        let state = self;
        let (rx, tx) = state.peers.mark_peer_connecting(addr)?;
        let counters = state
            .peers
            .with_peer(addr, |p| p.stats.counters.clone())
            .context("bug: peer not found")?;

        let handler = PeerHandler {
            addr,
            on_bitfield_notify: Default::default(),
            unchoke_notify: Default::default(),
            locked: RwLock::new(PeerHandlerLocked { i_am_choked: true }),
            requests_sem: Semaphore::new(0),
            state: state.clone(),
            tx,
            counters,
        };
        let options = PeerConnectionOptions {
            connect_timeout: state.meta.options.peer_connect_timeout,
            read_write_timeout: state.meta.options.peer_read_write_timeout,
            ..Default::default()
        };
        let peer_connection = PeerConnection::new(
            addr,
            state.meta.info_hash,
            state.meta.peer_id,
            &handler,
            Some(options),
            state.meta.spawner,
        );
        let requester = handler.task_peer_chunk_requester();

        handler
            .counters
            .outgoing_connection_attempts
            .fetch_add(1, Ordering::Relaxed);
        let res = tokio::select! {
            r = requester => {r}
            r = peer_connection.manage_peer_outgoing(rx) => {r}
        };

        match res {
            // We disconnected the peer ourselves as we don't need it
            Ok(()) => {
                handler.on_peer_died(None)?;
            }
            Err(e) => {
                debug!("error managing peer: {:#}", e);
                handler.on_peer_died(Some(e))?;
            }
        }
        drop(permit);
        Ok::<_, anyhow::Error>(())
    }

    async fn task_peer_adder(
        self: Arc<Self>,
        mut peer_queue_rx: UnboundedReceiver<SocketAddr>,
    ) -> anyhow::Result<()> {
        let state = self;
        loop {
            let addr = peer_queue_rx.recv().await.context("torrent closed")?;
            if state.is_finished() {
                debug!("ignoring peer {} as we are finished", addr);
                state.peers.mark_peer_not_needed(addr);
                continue;
            }

            let permit = state.peer_semaphore.clone().acquire_owned().await?;
            state.spawn(
                error_span!(parent: state.meta.span.clone(), "manage_peer", peer = addr.to_string()),
                state.clone().task_manage_outgoing_peer(addr, permit),
            );
        }
    }

    pub fn meta(&self) -> &ManagedTorrentInfo {
        &self.meta
    }

    pub fn info(&self) -> &TorrentMetaV1Info<ByteString> {
        &self.meta.info
    }
    pub fn info_hash(&self) -> Id20 {
        self.meta.info_hash
    }
    pub fn peer_id(&self) -> Id20 {
        self.meta.peer_id
    }
    pub(crate) fn file_ops(&self) -> FileOps<'_, Sha1> {
        FileOps::new(&self.meta.info, &self.files, &self.lengths)
    }
    pub fn initially_needed(&self) -> u64 {
        self.initially_needed_bytes
    }

    pub(crate) fn lock_read(
        &self,
        reason: &'static str,
    ) -> TimedExistence<RwLockReadGuard<TorrentStateLocked>> {
        TimedExistence::new(timeit(reason, || self.locked.read()), reason)
    }
    pub(crate) fn lock_write(
        &self,
        reason: &'static str,
    ) -> TimedExistence<RwLockWriteGuard<TorrentStateLocked>> {
        TimedExistence::new(timeit(reason, || self.locked.write()), reason)
    }

    fn set_peer_live<B>(&self, handle: PeerHandle, h: Handshake<B>) {
        self.peers.with_peer_mut(handle, "set_peer_live", |p| {
            p.state
                .connecting_to_live(Id20::new(h.peer_id), &self.peers.stats);
        });
    }

    pub fn get_total_selected_bytes(&self) -> u64 {
        self.total_selected_bytes
    }

    pub fn get_uploaded_bytes(&self) -> u64 {
        self.stats.uploaded_bytes.load(Ordering::Relaxed)
    }
    pub fn get_downloaded_bytes(&self) -> u64 {
        self.stats
            .downloaded_and_checked_bytes
            .load(Ordering::Acquire)
    }

    pub fn get_approx_have_bytes(&self) -> u64 {
        self.stats.have_bytes.load(Ordering::Relaxed)
    }

    pub fn is_finished(&self) -> bool {
        self.get_left_to_download_bytes() == 0
    }

    pub fn get_left_to_download_bytes(&self) -> u64 {
        self.initially_needed_bytes - self.get_downloaded_bytes()
    }

    fn maybe_transmit_haves(&self, index: ValidPieceIndex) {
        let mut futures = Vec::new();

        for pe in self.peers.states.iter() {
            match &pe.value().state.get() {
                PeerState::Live(live) => {
                    if !live.peer_interested {
                        continue;
                    }

                    if live
                        .bitfield
                        .get(index.get() as usize)
                        .map(|v| *v)
                        .unwrap_or(false)
                    {
                        continue;
                    }

                    let tx = live.tx.downgrade();
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

        // We don't want to remember this task as there may be too many.
        self.spawn(
            error_span!(
                parent: self.meta.span.clone(),
                "transmit_haves",
                piece = index.get(),
                count = unordered.len()
            ),
            async move {
                while unordered.next().await.is_some() {}
                Ok(())
            },
        );
    }

    pub(crate) fn add_peer_if_not_seen(&self, addr: SocketAddr) -> anyhow::Result<bool> {
        match self.peers.add_if_not_seen(addr) {
            Some(handle) => handle,
            None => return Ok(false),
        };

        self.peer_queue_tx.send(addr)?;
        Ok(true)
    }

    pub fn stats_snapshot(&self) -> StatsSnapshot {
        use Ordering::*;
        let downloaded_bytes = self.stats.downloaded_and_checked_bytes.load(Relaxed);
        StatsSnapshot {
            downloaded_and_checked_bytes: downloaded_bytes,
            downloaded_and_checked_pieces: self.stats.downloaded_and_checked_pieces.load(Relaxed),
            fetched_bytes: self.stats.fetched_bytes.load(Relaxed),
            uploaded_bytes: self.stats.uploaded_bytes.load(Relaxed),
            total_piece_download_ms: self.stats.total_piece_download_ms.load(Relaxed),
            peer_stats: self.peers.stats(),
        }
    }

    pub fn per_peer_stats_snapshot(&self, filter: PeerStatsFilter) -> PeerStatsSnapshot {
        PeerStatsSnapshot {
            peers: self
                .peers
                .states
                .iter()
                .filter(|e| filter.state.matches(e.value().state.get()))
                .map(|e| (e.key().to_string(), e.value().into()))
                .collect(),
        }
    }

    pub async fn wait_until_completed(&self) {
        if self.is_finished() {
            return;
        }
        self.finished_notify.notified().await;
    }

    pub fn pause(&self) -> anyhow::Result<TorrentStatePaused> {
        self.cancellation_token.cancel();

        let mut g = self.locked.write();

        let files = self
            .files
            .iter()
            .map(|f| {
                let mut f = f.lock();
                let dummy = dummy_file()?;
                let f = std::mem::replace(&mut *f, dummy);
                Ok::<_, anyhow::Error>(Arc::new(Mutex::new(f)))
            })
            .try_collect()?;

        let filenames = self.filenames.clone();

        let mut chunk_tracker = g
            .chunks
            .take()
            .context("bug: pausing already paused torrent")?;
        for piece_id in g.inflight_pieces.keys().copied() {
            chunk_tracker.mark_piece_broken_if_not_have(piece_id);
        }
        let have_bytes = chunk_tracker.calc_have_bytes();
        let needed_bytes = chunk_tracker.calc_needed_bytes();

        // g.chunks;
        Ok(TorrentStatePaused {
            info: self.meta.clone(),
            files,
            filenames,
            chunk_tracker,
            have_bytes,
            needed_bytes,
        })
    }

    fn on_fatal_error(&self, e: anyhow::Error) -> anyhow::Result<()> {
        let mut g = self.lock_write("fatal_error");
        let tx = g
            .fatal_errors_tx
            .take()
            .context("fatal_errors_tx already taken")?;
        let res = anyhow::anyhow!("fatal error: {:?}", e);
        if tx.send(e).is_err() {
            warn!("there's nowhere to send fatal error, receiver is dead");
        }
        Err(res)
    }
}

struct PeerHandlerLocked {
    pub i_am_choked: bool,
}

// All peer state that would never be used by other actors should pe put here.
// This state tracks a live peer.
struct PeerHandler {
    state: Arc<TorrentStateLive>,
    counters: Arc<AtomicPeerCounters>,
    // Semantically, we don't need an RwLock here, as this is only requested from
    // one future (requester + manage_peer).
    //
    // However as PeerConnectionHandler takes &self everywhere, we need shared mutability.
    // RefCell would do, but tokio is unhappy when we use it.
    locked: RwLock<PeerHandlerLocked>,

    // This is used to unpause chunk requester once the bitfield
    // is received.
    on_bitfield_notify: Notify,

    // This is used to unpause after we were choked.
    unchoke_notify: Notify,

    // This is used to limit the number of chunk requests we send to a peer at a time.
    requests_sem: Semaphore,

    addr: SocketAddr,

    tx: PeerTx,
}

impl<'a> PeerConnectionHandler for &'a PeerHandler {
    fn on_connected(&self, connection_time: Duration) {
        self.counters
            .outgoing_connections
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .total_time_connecting_ms
            .fetch_add(connection_time.as_millis() as u64, Ordering::Relaxed);
    }
    fn on_received_message(&self, message: Message<ByteBuf<'_>>) -> anyhow::Result<()> {
        match message {
            Message::Request(request) => {
                self.on_download_request(request)
                    .context("on_download_request")?;
            }
            Message::Bitfield(b) => self
                .on_bitfield(b.clone_to_owned())
                .context("on_bitfield")?,
            Message::Choke => self.on_i_am_choked(),
            Message::Unchoke => self.on_i_am_unchoked(),
            Message::Interested => self.on_peer_interested(),
            Message::Piece(piece) => self.on_received_piece(piece).context("on_received_piece")?,
            Message::KeepAlive => {
                trace!("keepalive received");
            }
            Message::Have(h) => self.on_have(h),
            Message::NotInterested => {
                trace!("received \"not interested\", but we don't process it yet")
            }
            Message::Cancel(_) => {
                trace!("received \"cancel\", but we don't process it yet")
            }
            message => {
                warn!("received unsupported message {:?}, ignoring", message);
            }
        };
        Ok(())
    }

    fn serialize_bitfield_message_to_buf(&self, buf: &mut Vec<u8>) -> anyhow::Result<usize> {
        let g = self.state.lock_read("serialize_bitfield_message_to_buf");
        let msg = Message::Bitfield(ByteBuf(g.get_chunks()?.get_have_pieces().as_raw_slice()));
        let len = msg.serialize(buf, &|| None)?;
        trace!("sending: {:?}, length={}", &msg, len);
        Ok(len)
    }

    fn on_handshake<B>(&self, handshake: Handshake<B>) -> anyhow::Result<()> {
        self.state.set_peer_live(self.addr, handshake);
        self.tx
            .send(WriterRequest::Message(MessageOwned::Unchoke))?;
        Ok(())
    }

    fn on_uploaded_bytes(&self, bytes: u32) {
        self.state
            .stats
            .uploaded_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    fn read_chunk(&self, chunk: &ChunkInfo, buf: &mut [u8]) -> anyhow::Result<()> {
        self.state.file_ops().read_chunk(self.addr, chunk, buf)
    }

    fn on_extended_handshake(&self, _: &ExtendedHandshake<ByteBuf>) -> anyhow::Result<()> {
        Ok(())
    }

    fn get_have_bytes(&self) -> u64 {
        self.state.get_approx_have_bytes()
    }
}

impl PeerHandler {
    fn on_peer_died(self, error: Option<anyhow::Error>) -> anyhow::Result<()> {
        let peers = &self.state.peers;
        let pstats = &peers.stats;
        let handle = self.addr;
        let mut pe = match peers.states.get_mut(&handle) {
            Some(peer) => TimedExistence::new(peer, "on_peer_died"),
            None => {
                warn!("bug: peer not found in table. Forgetting it forever");
                return Ok(());
            }
        };
        let prev = pe.value_mut().state.take(pstats);

        match prev {
            PeerState::Connecting(_) => {}
            PeerState::Live(live) => {
                let mut g = self.state.lock_write("mark_chunk_requests_canceled");
                for req in live.inflight_requests {
                    debug!(
                        "peer dead, marking chunk request cancelled, index={}, chunk={}",
                        req.piece.get(),
                        req.chunk
                    );
                    g.get_chunks_mut()?
                        .mark_chunk_request_cancelled(req.piece, req.chunk);
                }
            }
            PeerState::NotNeeded => {
                // Restore it as std::mem::take() replaced it above.
                pe.value_mut().state.set(PeerState::NotNeeded, pstats);
                return Ok(());
            }
            s @ PeerState::Queued | s @ PeerState::Dead => {
                warn!("bug: peer was in a wrong state {s:?}, ignoring it forever");
                // Prevent deadlocks.
                drop(pe);
                self.state.peers.drop_peer(handle);
                return Ok(());
            }
        };

        let _error = match error {
            Some(e) => e,
            None => {
                trace!("peer died without errors, not re-queueing");
                pe.value_mut().state.set(PeerState::NotNeeded, pstats);
                return Ok(());
            }
        };

        self.counters.errors.fetch_add(1, Ordering::Relaxed);

        if self.state.is_finished() {
            trace!("torrent finished, not re-queueing");
            pe.value_mut().state.set(PeerState::NotNeeded, pstats);
            return Ok(());
        }

        pe.value_mut().state.set(PeerState::Dead, pstats);

        let backoff = pe.value_mut().stats.backoff.next_backoff();

        // Prevent deadlocks.
        drop(pe);

        if let Some(dur) = backoff {
            self.state.clone().spawn(
                error_span!(
                    parent: self.state.meta.span.clone(),
                    "wait_for_peer",
                    peer = handle.to_string(),
                    duration = format!("{dur:?}")
                ),
                async move {
                    tokio::time::sleep(dur).await;
                    self.state
                        .peers
                        .with_peer_mut(handle, "dead_to_queued", |peer| {
                            match peer.state.get() {
                                PeerState::Dead => {
                                    peer.state.set(PeerState::Queued, &self.state.peers.stats)
                                }
                                other => bail!(
                                    "peer is in unexpected state: {}. Expected dead",
                                    other.name()
                                ),
                            };
                            Ok(())
                        })
                        .context("bug: peer disappeared")??;
                    self.state.peer_queue_tx.send(handle)?;
                    Ok::<_, anyhow::Error>(())
                },
            );
        } else {
            debug!("dropping peer, backoff exhausted");
            self.state.peers.drop_peer(handle);
        };
        Ok(())
    }

    fn reserve_next_needed_piece(&self) -> anyhow::Result<Option<ValidPieceIndex>> {
        // TODO: locking one inside the other in different order results in deadlocks.
        self.state
            .peers
            .with_live_mut(self.addr, "reserve_next_needed_piece", |live| {
                if self.locked.read().i_am_choked {
                    debug!("we are choked, can't reserve next piece");
                    return Ok(None);
                }
                let mut g = self.state.lock_write("reserve_next_needed_piece");

                let n = {
                    let mut n_opt = None;
                    let bf = &live.bitfield;
                    for n in g.get_chunks()?.iter_needed_pieces() {
                        if bf.get(n).map(|v| *v) == Some(true) {
                            n_opt = Some(n);
                            break;
                        }
                    }

                    let n_opt = match n_opt {
                        Some(n_opt) => n_opt,
                        None => return Ok(None),
                    };

                    self.state
                        .lengths
                        .validate_piece_index(n_opt as u32)
                        .context("bug: invalid piece")?
                };
                g.inflight_pieces.insert(
                    n,
                    InflightPiece {
                        peer: self.addr,
                        started: Instant::now(),
                    },
                );
                g.get_chunks_mut()?.reserve_needed_piece(n);
                Ok(Some(n))
            })
            .transpose()
            .map(|r| r.flatten())
    }

    /// Try to steal a piece from a slower peer. Threshold is
    /// "how many times is my average download speed faster to be able to steal".
    ///
    /// If this returns, an existing in-flight piece was marked to be ours.
    fn try_steal_old_slow_piece(&self, threshold: f64) -> Option<ValidPieceIndex> {
        let my_avg_time = match self.counters.average_piece_download_time() {
            Some(t) => t,
            None => return None,
        };

        let mut g = self.state.lock_write("try_steal_old_slow_piece");
        let (idx, elapsed, piece_req) = g
            .inflight_pieces
            .iter_mut()
            // don't steal from myself
            .filter(|(_, r)| r.peer != self.addr)
            .map(|(p, r)| (p, r.started.elapsed(), r))
            .max_by_key(|(_, e, _)| *e)?;

        // heuristic for "too slow peer"
        if elapsed.as_secs_f64() > my_avg_time.as_secs_f64() * threshold {
            debug!(
                "will steal piece {} from {}: elapsed time {:?}, my avg piece time: {:?}",
                idx, piece_req.peer, elapsed, my_avg_time
            );
            piece_req.peer = self.addr;
            piece_req.started = Instant::now();
            return Some(*idx);
        }
        None
    }

    fn on_download_request(&self, request: Request) -> anyhow::Result<()> {
        let piece_index = match self.state.lengths.validate_piece_index(request.index) {
            Some(p) => p,
            None => {
                anyhow::bail!(
                    "received {:?}, but it is not a valid chunk request (piece index is invalid). Ignoring.",
                    request
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
                    "received {:?}, but it is not a valid chunk request (chunk data is invalid). Ignoring.",
                    request
                );
            }
        };

        if !self
            .state
            .lock_read("is_chunk_ready_to_upload")
            .get_chunks()?
            .is_chunk_ready_to_upload(&chunk_info)
        {
            anyhow::bail!(
                "got request for a chunk that is not ready to upload. chunk {:?}",
                &chunk_info
            );
        }

        // TODO: this is not super efficient as it does copying multiple times.
        // Theoretically, this could be done in the sending code, so that it reads straight into
        // the send buffer.
        let request = WriterRequest::ReadChunkRequest(chunk_info);
        trace!("sending {:?}", &request);
        Ok::<_, anyhow::Error>(self.tx.send(request)?)
    }

    fn on_have(&self, have: u32) {
        self.state
            .peers
            .with_live_mut(self.addr, "on_have", |live| {
                // If bitfield wasn't allocated yet, let's do it. Some clients start empty so they never
                // send bitfields.
                if live.bitfield.is_empty() {
                    live.bitfield = make_piece_bitfield(&self.state.lengths);
                }
                match live.bitfield.get_mut(have as usize) {
                    Some(mut v) => *v = true,
                    None => {
                        warn!("received have {} out of range", have);
                        return;
                    }
                };
                trace!("updated bitfield with have={}", have);
            });
        self.on_bitfield_notify.notify_waiters();
    }

    fn on_bitfield(&self, bitfield: ByteString) -> anyhow::Result<()> {
        if bitfield.len() != self.state.lengths.piece_bitfield_bytes() {
            anyhow::bail!(
                "dropping peer as its bitfield has unexpected size. Got {}, expected {}",
                bitfield.len(),
                self.state.lengths.piece_bitfield_bytes(),
            );
        }
        self.state
            .peers
            .update_bitfield_from_vec(self.addr, bitfield.0);
        self.on_bitfield_notify.notify_waiters();
        Ok(())
    }

    async fn wait_for_any_notify(&self, notify: &Notify, check: impl Fn() -> bool) {
        // To remove possibility of races, we first grab a token, then check
        // if we need it, and only if so, await.
        let notified = notify.notified();
        if check() {
            return;
        }
        notified.await;
    }

    async fn wait_for_bitfield(&self) {
        self.wait_for_any_notify(&self.on_bitfield_notify, || {
            self.state
                .peers
                .with_live(self.addr, |live| !live.bitfield.is_empty())
                .unwrap_or_default()
        })
        .await;
    }

    async fn wait_for_unchoke(&self) {
        self.wait_for_any_notify(&self.unchoke_notify, || !self.locked.read().i_am_choked)
            .await;
    }

    async fn task_peer_chunk_requester(&self) -> anyhow::Result<()> {
        let handle = self.addr;
        self.wait_for_bitfield().await;

        // TODO: this check needs to happen more often, we need to update our
        // interested state with the other side, for now we send it only once.
        if self.state.is_finished() {
            self.tx
                .send(WriterRequest::Message(MessageOwned::NotInterested))?;

            if self
                .state
                .peers
                .with_live(self.addr, |l| {
                    l.has_full_torrent(self.state.lengths.total_pieces() as usize)
                })
                .unwrap_or_default()
            {
                debug!("both peer and us have full torrent, disconnecting");
                self.tx.send(WriterRequest::Disconnect)?;
                // Sleep a bit to ensure this gets written to the network by manage_peer
                tokio::time::sleep(Duration::from_millis(100)).await;
                return Ok(());
            }
        } else {
            self.tx
                .send(WriterRequest::Message(MessageOwned::Interested))?;
        }

        loop {
            self.wait_for_unchoke().await;

            if self.state.is_finished() {
                debug!("nothing left to download, looping forever until manage_peer quits");
                loop {
                    tokio::time::sleep(Duration::from_secs(86400)).await;
                }
            }

            // Try steal a pice from a very slow peer first. Otherwise we might wait too long
            // to download early pieces.
            // Then try get the next one in queue.
            // Afterwards means we are close to completion, try stealing more aggressively.
            let next = match self
                .try_steal_old_slow_piece(10.)
                .map_or_else(|| self.reserve_next_needed_piece(), |v| Ok(Some(v)))?
                .or_else(|| self.try_steal_old_slow_piece(3.))
            {
                Some(next) => next,
                None => {
                    debug!("no pieces to request");
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    continue;
                }
            };

            for chunk in self.state.lengths.iter_chunk_infos(next) {
                let request = Request {
                    index: next.get(),
                    begin: chunk.offset,
                    length: chunk.size,
                };

                match self
                    .state
                    .peers
                    .with_live_mut(handle, "add chunk request", |live| {
                        live.inflight_requests.insert(InflightRequest::from(&chunk))
                    }) {
                    Some(true) => {}
                    Some(false) => {
                        // This request was already in-flight for this peer for this chunk.
                        // This might happen in theory, but not very likely.
                        //
                        // Example:
                        // someone stole a piece from us, and then died, the piece became "needed" again, and we reserved it
                        // all before the piece request was processed by us.
                        warn!("we already requested {:?} previously", chunk);
                        continue;
                    }
                    // peer died
                    None => return Ok(()),
                };

                loop {
                    match timeout(Duration::from_secs(10), self.requests_sem.acquire()).await {
                        Ok(acq) => break acq?.forget(),
                        Err(_) => continue,
                    };
                }

                if self
                    .tx
                    .send(WriterRequest::Message(MessageOwned::Request(request)))
                    .is_err()
                {
                    return Ok(());
                }
            }
        }
    }

    fn on_i_am_choked(&self) {
        self.locked.write().i_am_choked = true;
    }

    fn on_peer_interested(&self) {
        trace!("peer is interested");
        self.state.peers.mark_peer_interested(self.addr, true);
    }

    fn reopen_read_only(&self) -> anyhow::Result<()> {
        // Lock exclusive just in case to ensure in-flight operations finish.??
        let _guard = self.state.lock_write("reopen_read_only");

        for (file, filename) in self.state.files.iter().zip(self.state.filenames.iter()) {
            let mut g = file.lock();
            // this should close the original file
            // putting in a block just in case to guarantee drop.
            {
                *g = dummy_file()?;
            }
            *g = std::fs::OpenOptions::new()
                .read(true)
                .open(filename)
                .with_context(|| format!("error re-opening {:?} readonly", filename))?;
            debug!("reopened {:?} read-only", filename);
        }
        info!("reopened all torrent files in read-only mode");
        Ok(())
    }

    fn on_i_am_unchoked(&self) {
        trace!("we are unchoked");
        self.locked.write().i_am_choked = false;
        self.unchoke_notify.notify_waiters();
        self.requests_sem.add_permits(16);
    }

    fn on_received_piece(&self, piece: Piece<ByteBuf>) -> anyhow::Result<()> {
        let chunk_info = match self.state.lengths.chunk_info_from_received_piece(
            piece.index,
            piece.begin,
            piece.block.len() as u32,
        ) {
            Some(i) => i,
            None => {
                anyhow::bail!("peer sent us an invalid piece {:?}", &piece,);
            }
        };

        self.requests_sem.add_permits(1);

        // Peer chunk/byte counters.
        self.counters
            .fetched_bytes
            .fetch_add(piece.block.len() as u64, Ordering::Relaxed);
        self.counters.fetched_chunks.fetch_add(1, Ordering::Relaxed);

        // Global chunk/byte counters.
        self.state
            .stats
            .fetched_bytes
            .fetch_add(piece.block.len() as u64, Ordering::Relaxed);

        self.state
            .peers
            .with_live_mut(self.addr, "inflight_requests.remove", |h| {
                if !h
                    .inflight_requests
                    .remove(&InflightRequest::from(&chunk_info))
                {
                    anyhow::bail!(
                        "peer sent us a piece we did not ask. Requested pieces: {:?}. Got: {:?}",
                        &h.inflight_requests,
                        &piece,
                    );
                }
                Ok(())
            })
            .context("peer not found")??;

        let full_piece_download_time = {
            let mut g = self.state.lock_write("mark_chunk_downloaded");

            match g.inflight_pieces.get(&chunk_info.piece_index) {
                Some(InflightPiece { peer, .. }) if *peer == self.addr => {}
                Some(InflightPiece { peer, .. }) => {
                    debug!(
                        "in-flight piece {} was stolen by {}, ignoring",
                        chunk_info.piece_index, peer
                    );
                    return Ok(());
                }
                None => {
                    debug!(
                        "in-flight piece {} not found. it was probably completed by someone else",
                        chunk_info.piece_index
                    );
                    return Ok(());
                }
            };

            match g.get_chunks_mut()?.mark_chunk_downloaded(&piece) {
                Some(ChunkMarkingResult::Completed) => {
                    trace!("piece={} done, will write and checksum", piece.index,);
                    // This will prevent others from stealing it.
                    {
                        let piece = chunk_info.piece_index;
                        g.inflight_pieces.remove(&piece)
                    }
                    .map(|t| t.started.elapsed())
                }
                Some(ChunkMarkingResult::PreviouslyCompleted) => {
                    // TODO: we might need to send cancellations here.
                    debug!("piece={} was done by someone else, ignoring", piece.index,);
                    return Ok(());
                }
                Some(ChunkMarkingResult::NotCompleted) => None,
                None => {
                    anyhow::bail!(
                        "bogus data received: {:?}, cannot map this to a chunk, dropping peer",
                        piece
                    );
                }
            }
        };

        // By this time we reach here, no other peer can for this piece. All others, even if they steal pieces would
        // have fallen off above in one of the defensive checks.

        self.state
            .meta
            .spawner
            .spawn_block_in_place(move || {
                let index = piece.index;

                // TODO: in theory we should unmark the piece as downloaded here. But if there was a disk error, what
                // should we really do? If we unmark it, it will get requested forever...
                //
                // So let's just unwrap and abort.
                match self
                    .state
                    .file_ops()
                    .write_chunk(self.addr, &piece, &chunk_info)
                {
                    Ok(()) => {}
                    Err(e) => {
                        error!("FATAL: error writing chunk to disk: {:?}", e);
                        return self.state.on_fatal_error(e);
                    }
                }

                let full_piece_download_time = match full_piece_download_time {
                    Some(t) => t,
                    None => return Ok(()),
                };

                match self
                    .state
                    .file_ops()
                    .check_piece(self.addr, chunk_info.piece_index, &chunk_info)
                    .with_context(|| format!("error checking piece={index}"))?
                {
                    true => {
                        {
                            let mut g = self.state.lock_write("mark_piece_downloaded");
                            g.get_chunks_mut()?
                                .mark_piece_downloaded(chunk_info.piece_index);
                        }

                        // Global piece counters.
                        let piece_len =
                            self.state.lengths.piece_length(chunk_info.piece_index) as u64;
                        self.state
                            .stats
                            .downloaded_and_checked_bytes
                            // This counter is used to compute "is_finished", so using
                            // stronger ordering.
                            .fetch_add(piece_len, Ordering::Release);
                        self.state
                            .stats
                            .downloaded_and_checked_pieces
                            // This counter is used to compute "is_finished", so using
                            // stronger ordering.
                            .fetch_add(1, Ordering::Release);
                        self.state
                            .stats
                            .have_bytes
                            .fetch_add(piece_len, Ordering::Relaxed);
                        self.state.stats.total_piece_download_ms.fetch_add(
                            full_piece_download_time.as_millis() as u64,
                            Ordering::Relaxed,
                        );

                        // Per-peer piece counters.
                        self.counters
                            .on_piece_downloaded(piece_len, full_piece_download_time);
                        self.state.peers.reset_peer_backoff(self.addr);

                        debug!("piece={} successfully downloaded and verified", index);

                        if self.state.is_finished() {
                            info!("torrent finished downloading");
                            self.state.finished_notify.notify_waiters();
                            self.disconnect_all_peers_that_have_full_torrent();
                            self.reopen_read_only()?;
                        }

                        self.state.maybe_transmit_haves(chunk_info.piece_index);
                    }
                    false => {
                        warn!("checksum for piece={} did not validate", index,);
                        self.state
                            .lock_write("mark_piece_broken")
                            .get_chunks_mut()?
                            .mark_piece_broken_if_not_have(chunk_info.piece_index);
                    }
                };
                Ok::<_, anyhow::Error>(())
            })
            .with_context(|| format!("error processing received chunk {chunk_info:?}"))?;
        Ok(())
    }

    fn disconnect_all_peers_that_have_full_torrent(&self) {
        for mut pe in self.state.peers.states.iter_mut() {
            if let PeerState::Live(l) = pe.value().state.get() {
                if l.has_full_torrent(self.state.lengths.total_pieces() as usize) {
                    let prev = pe.value_mut().state.set_not_needed(&self.state.peers.stats);
                    let _ = prev
                        .take_live_no_counters()
                        .unwrap()
                        .tx
                        .send(WriterRequest::Disconnect);
                }
            }
        }
    }
}
