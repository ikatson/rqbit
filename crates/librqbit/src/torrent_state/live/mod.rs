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
    collections::{HashMap, HashSet},
    net::SocketAddr,
    num::NonZeroU32,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{bail, Context};
use backoff::backoff::Backoff;
use buffers::{ByteBuf, ByteBufOwned};
use clone_to_owned::CloneToOwned;
use librqbit_core::{
    constants::CHUNK_SIZE,
    hash_id::Id20,
    lengths::{ChunkInfo, Lengths, ValidPieceIndex},
    spawn_utils::spawn_with_cancel,
    speed_estimator::SpeedEstimator,
    torrent_metainfo::TorrentMetaV1Info,
};
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use peer_binary_protocol::{
    extended::{
        self, handshake::ExtendedHandshake, ut_metadata::UtMetadata, ut_pex::UtPex, ExtendedMessage,
    },
    Handshake, Message, MessageOwned, Piece, Request,
};
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    Notify, OwnedSemaphorePermit, Semaphore,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, error_span, info, trace, warn, Instrument};

use crate::{
    chunk_tracker::{ChunkMarkingResult, ChunkTracker, HaveNeededSelected},
    file_ops::FileOps,
    limits::Limits,
    peer_connection::{
        PeerConnection, PeerConnectionHandler, PeerConnectionOptions, WriterRequest,
    },
    session::CheckedIncomingConnection,
    session_stats::atomic::AtomicSessionStats,
    torrent_state::{peer::Peer, utils::atomic_inc},
    type_aliases::{DiskWorkQueueSender, FilePriorities, FileStorage, PeerHandle, BF},
};

use self::{
    peer::{
        stats::{
            atomic::PeerCountersAtomic as AtomicPeerCounters,
            snapshot::{PeerStatsFilter, PeerStatsSnapshot},
        },
        PeerRx, PeerState, PeerTx,
    },
    peers::PeerStates,
    stats::{atomic::AtomicStats, snapshot::StatsSnapshot},
};

use super::{
    paused::TorrentStatePaused,
    streaming::TorrentStreams,
    utils::{timeit, TimedExistence},
    ManagedTorrentShared, TorrentMetadata,
};

#[derive(Debug)]
struct InflightPiece {
    peer: PeerHandle,
    started: Instant,
}

fn make_piece_bitfield(lengths: &Lengths) -> BF {
    BF::from_boxed_slice(vec![0; lengths.piece_bitfield_bytes()].into_boxed_slice())
}

pub(crate) struct TorrentStateLocked {
    // What chunks we have and need.
    // If this is None, the torrent was paused, and this live state is useless, and needs to be dropped.
    pub(crate) chunks: Option<ChunkTracker>,

    // The sorted file list in which order to download them.
    file_priorities: FilePriorities,

    // At a moment in time, we are expecting a piece from only one peer.
    // inflight_pieces stores this information.
    inflight_pieces: HashMap<ValidPieceIndex, InflightPiece>,

    // If this is None, then it was already used
    fatal_errors_tx: Option<tokio::sync::oneshot::Sender<anyhow::Error>>,

    unflushed_bitv_bytes: u64,
}

impl TorrentStateLocked {
    pub(crate) fn get_chunks(&self) -> anyhow::Result<&ChunkTracker> {
        self.chunks
            .as_ref()
            .context("chunk tracker empty, torrent was paused")
    }

    pub(crate) fn get_chunks_mut(&mut self) -> anyhow::Result<&mut ChunkTracker> {
        self.chunks
            .as_mut()
            .context("chunk tracker empty, torrent was paused")
    }

    fn try_flush_bitv(&mut self) {
        if self.unflushed_bitv_bytes == 0 {
            return;
        }
        trace!("trying to flush bitfield");
        if let Some(Err(e)) = self
            .chunks
            .as_mut()
            .map(|ct| ct.get_have_pieces_mut().flush())
        {
            warn!(error=?e, "error flushing bitfield");
        } else {
            trace!("flushed bitfield");
            self.unflushed_bitv_bytes = 0;
        }
    }
}

const FLUSH_BITV_EVERY_BYTES: u64 = 16 * 1024 * 1024;

pub struct TorrentStateLive {
    peers: PeerStates,
    shared: Arc<ManagedTorrentShared>,
    metadata: Arc<TorrentMetadata>,
    locked: RwLock<TorrentStateLocked>,

    pub(crate) files: FileStorage,

    per_piece_locks: Vec<RwLock<()>>,

    stats: AtomicStats,
    lengths: Lengths,

    // Limits how many active (occupying network resources) peers there are at a moment in time.
    peer_semaphore: Arc<Semaphore>,

    // The queue for peer manager to connect to them.
    peer_queue_tx: UnboundedSender<SocketAddr>,

    finished_notify: Notify,
    new_pieces_notify: Notify,

    down_speed_estimator: SpeedEstimator,
    up_speed_estimator: SpeedEstimator,
    cancellation_token: CancellationToken,

    session_stats: Arc<AtomicSessionStats>,

    pub(crate) streams: Arc<TorrentStreams>,
    have_broadcast_tx: tokio::sync::broadcast::Sender<ValidPieceIndex>,

    ratelimit_upload_tx: tokio::sync::mpsc::UnboundedSender<(
        tokio::sync::mpsc::UnboundedSender<WriterRequest>,
        ChunkInfo,
    )>,
    ratelimits: Limits,
}

impl TorrentStateLive {
    pub(crate) fn new(
        paused: TorrentStatePaused,
        fatal_errors_tx: tokio::sync::oneshot::Sender<anyhow::Error>,
        cancellation_token: CancellationToken,
    ) -> anyhow::Result<Arc<Self>> {
        let (peer_queue_tx, peer_queue_rx) = unbounded_channel();
        let session = paused
            .shared
            .session
            .upgrade()
            .context("session is dead, cannot start torrent")?;
        let session_stats = session.stats.atomic.clone();
        let down_speed_estimator = SpeedEstimator::default();
        let up_speed_estimator = SpeedEstimator::default();

        let have_bytes = paused.chunk_tracker.get_hns().have_bytes;
        let lengths = *paused.chunk_tracker.get_lengths();

        // TODO: make it configurable
        let file_priorities = {
            let mut pri = (0..paused.metadata.file_infos.len()).collect::<Vec<usize>>();
            // sort by filename, cause many torrents have random sort order.
            pri.sort_unstable_by_key(|id| {
                paused
                    .metadata
                    .file_infos
                    .get(*id)
                    .map(|fi| fi.relative_filename.as_path())
            });
            pri
        };

        let (have_broadcast_tx, _) = tokio::sync::broadcast::channel(128);

        let (ratelimit_upload_tx, ratelimit_upload_rx) = tokio::sync::mpsc::unbounded_channel::<(
            tokio::sync::mpsc::UnboundedSender<WriterRequest>,
            ChunkInfo,
        )>();
        let ratelimits = Limits::new(paused.shared.options.ratelimits);

        let state = Arc::new(TorrentStateLive {
            shared: paused.shared.clone(),
            metadata: paused.metadata.clone(),
            peers: PeerStates {
                session_stats: session_stats.clone(),
                stats: Default::default(),
                states: Default::default(),
                live_outgoing_peers: Default::default(),
            },
            locked: RwLock::new(TorrentStateLocked {
                chunks: Some(paused.chunk_tracker),
                // TODO: move under per_piece_locks?
                inflight_pieces: Default::default(),
                file_priorities,
                fatal_errors_tx: Some(fatal_errors_tx),
                unflushed_bitv_bytes: 0,
            }),
            files: paused.files,
            stats: AtomicStats {
                have_bytes: AtomicU64::new(have_bytes),
                ..Default::default()
            },
            lengths,
            peer_semaphore: Arc::new(Semaphore::new(128)),
            new_pieces_notify: Notify::new(),
            peer_queue_tx,
            finished_notify: Notify::new(),
            down_speed_estimator,
            up_speed_estimator,
            cancellation_token,
            have_broadcast_tx,
            session_stats,
            streams: paused.streams,
            per_piece_locks: (0..lengths.total_pieces())
                .map(|_| RwLock::new(()))
                .collect(),
            ratelimit_upload_tx,
            ratelimits,
        });

        state.spawn(
            error_span!(parent: state.shared.span.clone(), "speed_estimator_updater"),
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
                        let remaining = state.locked.read().get_chunks()?.get_remaining_bytes();
                        state
                            .down_speed_estimator
                            .add_snapshot(fetched, Some(remaining), now);
                        state
                            .up_speed_estimator
                            .add_snapshot(stats.uploaded_bytes, None, now);
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            },
        );

        state.spawn(
            error_span!(parent: state.shared.span.clone(), "peer_adder"),
            state.clone().task_peer_adder(peer_queue_rx),
        );

        state.spawn(
            error_span!(parent: state.shared.span.clone(), "upload_scheduler"),
            state.clone().task_upload_scheduler(ratelimit_upload_rx),
        );
        Ok(state)
    }

    #[track_caller]
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

    fn disk_work_tx(&self) -> Option<&DiskWorkQueueSender> {
        self.shared.options.disk_write_queue.as_ref()
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
                debug!("limit of live peers reached, dropping incoming peer");
                self.peers.with_peer(checked_peer.addr, |p| {
                    atomic_inc(&p.stats.counters.incoming_connections);
                });
                return Ok(());
            }
        };

        let counters = match self.peers.states.entry(checked_peer.addr) {
            Entry::Occupied(mut occ) => {
                let peer = occ.get_mut();
                peer.incoming_connection(
                    Id20::new(checked_peer.handshake.peer_id),
                    tx.clone(),
                    &self.peers,
                )
                .context("peer already existed")?;
                peer.stats.counters.clone()
            }
            Entry::Vacant(vac) => {
                atomic_inc(&self.peers.stats.seen);
                let peer = Peer::new_live_for_incoming_connection(
                    *vac.key(),
                    Id20::new(checked_peer.handshake.peer_id),
                    tx.clone(),
                    &self.peers,
                );
                let counters = peer.stats.counters.clone();
                vac.insert(peer);
                counters
            }
        };
        atomic_inc(&counters.incoming_connections);

        self.spawn(
            error_span!(
                parent: self.shared.span.clone(),
                "manage_incoming_peer",
                addr = %checked_peer.addr
            ),
            aframe!(self
                .clone()
                .task_manage_incoming_peer(checked_peer, counters, tx, rx, permit)),
        );
        Ok(())
    }

    async fn task_upload_scheduler(
        self: Arc<Self>,
        mut rx: tokio::sync::mpsc::UnboundedReceiver<(
            tokio::sync::mpsc::UnboundedSender<WriterRequest>,
            ChunkInfo,
        )>,
    ) -> anyhow::Result<()> {
        while let Some((tx, ci)) = rx.recv().await {
            self.ratelimits
                .prepare_for_upload(NonZeroU32::new(ci.size).unwrap())
                .await?;
            if let Some(session) = self.shared.session.upgrade() {
                session
                    .ratelimits
                    .prepare_for_upload(NonZeroU32::new(ci.size).unwrap())
                    .await?;
            }
            let _ = tx.send(WriterRequest::ReadChunkRequest(ci));
        }
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
            incoming: true,
            on_bitfield_notify: Default::default(),
            unchoke_notify: Default::default(),
            locked: RwLock::new(PeerHandlerLocked { i_am_choked: true }),
            requests_sem: Semaphore::new(0),
            state: self.clone(),
            tx,
            counters,
            first_message_received: AtomicBool::new(false),
        };
        let options = PeerConnectionOptions {
            connect_timeout: self.shared.options.peer_connect_timeout,
            read_write_timeout: self.shared.options.peer_read_write_timeout,
            ..Default::default()
        };
        let peer_connection = PeerConnection::new(
            checked_peer.addr,
            self.shared.info_hash,
            self.shared.peer_id,
            &handler,
            Some(options),
            self.shared.spawner,
            self.shared.connector.clone(),
        );
        let requester = handler.task_peer_chunk_requester();

        let res = tokio::select! {
            r = requester => {r}
            r = peer_connection.manage_peer_incoming(
                rx,
                checked_peer.read_buf,
                checked_peer.handshake,
                checked_peer.reader,
                checked_peer.writer,
                self.have_broadcast_tx.subscribe()
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
            incoming: false,
            on_bitfield_notify: Default::default(),
            unchoke_notify: Default::default(),
            locked: RwLock::new(PeerHandlerLocked { i_am_choked: true }),
            requests_sem: Semaphore::new(0),
            state: state.clone(),
            tx,
            counters,
            first_message_received: AtomicBool::new(false),
        };
        let options = PeerConnectionOptions {
            connect_timeout: state.shared.options.peer_connect_timeout,
            read_write_timeout: state.shared.options.peer_read_write_timeout,
            ..Default::default()
        };
        let peer_connection = PeerConnection::new(
            addr,
            state.shared.info_hash,
            state.shared.peer_id,
            &handler,
            Some(options),
            state.shared.spawner,
            state.shared.connector.clone(),
        );
        let requester = aframe!(handler
            .task_peer_chunk_requester()
            .instrument(error_span!("chunk_requester")));
        let conn_manager = aframe!(peer_connection
            .manage_peer_outgoing(rx, state.have_broadcast_tx.subscribe())
            .instrument(error_span!("peer_connection")));

        handler
            .counters
            .outgoing_connection_attempts
            .fetch_add(1, Ordering::Relaxed);
        let res = tokio::select! {
            r = requester => {r}
            r = conn_manager => {r}
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
            if state.shared.options.disable_upload() && state.is_finished_and_no_active_streams() {
                debug!("ignoring peer {} as we are finished", addr);
                state.peers.mark_peer_not_needed(addr);
                continue;
            }

            let outgoing_ip = addr.ip();
            let is_blocked_ip = state.shared.session.upgrade().map_or_else(
                || false,
                |session| session.blocklist.is_blocked(outgoing_ip),
            );

            if is_blocked_ip {
                info!("Outgoing ip {outgoing_ip} for peer is in blocklist skipping");
                continue;
            }

            let permit = state.peer_semaphore.clone().acquire_owned().await?;
            state.spawn(
                error_span!(parent: state.shared.span.clone(), "manage_peer", peer = addr.to_string()),
                aframe!(state.clone().task_manage_outgoing_peer(addr, permit)),
            );
        }
    }

    pub fn torrent(&self) -> &ManagedTorrentShared {
        &self.shared
    }

    pub fn info(&self) -> &TorrentMetaV1Info<ByteBufOwned> {
        &self.metadata.info
    }
    pub fn info_hash(&self) -> Id20 {
        self.shared.info_hash
    }
    pub fn peer_id(&self) -> Id20 {
        self.shared.peer_id
    }
    pub(crate) fn file_ops(&self) -> FileOps<'_> {
        FileOps::new(
            &self.metadata.info,
            &*self.files,
            &self.metadata.file_infos,
            &self.lengths,
        )
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
            p.connecting_to_live(Id20::new(h.peer_id), &self.peers);
        });
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

    pub fn get_hns(&self) -> Option<HaveNeededSelected> {
        self.lock_read("get_hns")
            .get_chunks()
            .ok()
            .map(|c| *c.get_hns())
    }

    fn transmit_haves(&self, index: ValidPieceIndex) {
        let _ = self.have_broadcast_tx.send(index);
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
                .filter(|e| filter.state.matches(e.value().get_state()))
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

        // It should be impossible to make a fatal error after pausing.
        g.fatal_errors_tx.take();

        let mut chunk_tracker = g
            .chunks
            .take()
            .context("bug: pausing already paused torrent")?;
        for piece_id in g.inflight_pieces.keys().copied() {
            chunk_tracker.mark_piece_broken_if_not_have(piece_id);
        }

        // g.chunks;
        Ok(TorrentStatePaused {
            shared: self.shared.clone(),
            metadata: self.metadata.clone(),
            files: self.files.take()?,
            chunk_tracker,
            streams: self.streams.clone(),
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

    pub(crate) fn update_only_files(&self, only_files: &HashSet<usize>) -> anyhow::Result<()> {
        let mut g = self.lock_write("update_only_files");
        let ct = g.get_chunks_mut()?;
        let hns =
            ct.update_only_files(self.metadata.file_infos.iter().map(|f| f.len), only_files)?;
        if !hns.finished() {
            self.reconnect_all_not_needed_peers();
        }
        Ok(())
    }

    // If we have all selected pieces but not necessarily all pieces.
    pub(crate) fn is_finished(&self) -> bool {
        self.get_hns().map(|h| h.finished()).unwrap_or_default()
    }

    fn has_active_streams_unfinished_files(&self, state: &TorrentStateLocked) -> bool {
        let chunks = match state.get_chunks() {
            Ok(c) => c,
            Err(_) => return false,
        };
        self.streams
            .streamed_file_ids()
            .any(|file_id| !chunks.is_file_finished(&self.metadata.file_infos[file_id]))
    }

    // We might have the torrent "finished" i.e. no selected files. But if someone is streaming files despite
    // them being selected, we aren't fully "finished".
    fn is_finished_and_no_active_streams(&self) -> bool {
        self.is_finished()
            && !self.has_active_streams_unfinished_files(
                &self.lock_read("is_finished_and_dont_need_peers"),
            )
    }

    fn on_piece_completed(&self, id: ValidPieceIndex) -> anyhow::Result<()> {
        if let Err(e) = self.files.on_piece_completed(id) {
            debug!(?id, "file storage errored in on_piece_completed(): {e:#}");
        }
        let mut g = self.lock_write("on_piece_completed");
        let locked = &mut **g;
        let chunks = locked.get_chunks_mut()?;

        // if we have all the pieces of the file, reopen it read only
        for (idx, file_info) in self
            .metadata
            .file_infos
            .iter()
            .enumerate()
            .skip_while(|(_, fi)| !fi.piece_range.contains(&id.get()))
            .take_while(|(_, fi)| fi.piece_range.contains(&id.get()))
        {
            let _remaining = chunks.update_file_have_on_piece_completed(id, idx, file_info);
        }

        self.streams
            .wake_streams_on_piece_completed(id, &self.metadata.lengths);

        locked.unflushed_bitv_bytes += self.metadata.lengths.piece_length(id) as u64;
        if locked.unflushed_bitv_bytes >= FLUSH_BITV_EVERY_BYTES {
            locked.try_flush_bitv()
        }

        let chunks = locked.get_chunks()?;
        if chunks.is_finished() {
            if chunks.get_selected_pieces()[id.get_usize()] {
                locked.try_flush_bitv();
                info!("torrent finished downloading");
            }
            self.finished_notify.notify_waiters();

            if !self.has_active_streams_unfinished_files(locked) {
                // prevent deadlocks.
                drop(g);
                // There is not poing being connected to peers that have all the torrent, when
                // we don't need anything from them, and they don't need anything from us.
                self.disconnect_all_peers_that_have_full_torrent();
            }
        }
        Ok(())
    }

    fn disconnect_all_peers_that_have_full_torrent(&self) {
        for mut pe in self.peers.states.iter_mut() {
            if let PeerState::Live(l) = pe.value().get_state() {
                if l.has_full_torrent(self.lengths.total_pieces() as usize) {
                    let prev = pe.value_mut().set_not_needed(&self.peers);
                    let _ = prev
                        .take_live_no_counters()
                        .unwrap()
                        .tx
                        .send(WriterRequest::Disconnect(Ok(())));
                }
            }
        }
    }

    pub(crate) fn reconnect_all_not_needed_peers(&self) {
        self.peers
            .states
            .iter_mut()
            .filter_map(|mut p| p.value_mut().reconnect_not_needed_peer(&self.peers))
            .map(|socket_addr| self.peer_queue_tx.send(socket_addr))
            .take_while(|r| r.is_ok())
            .last();
    }

    async fn task_send_pex_to_peer(
        self: Arc<Self>,
        _peer_addr: SocketAddr,
        tx: PeerTx,
    ) -> anyhow::Result<()> {
        // As per BEP 11 we should not send more than 50 peers at once
        // (here it also applies to fist message, should be OK as we anyhow really have more)
        const MAX_SENT_PEERS: usize = 50;
        // As per BEP 11 recommended interval is min 60 seconds
        const PEX_MESSAGE_INTERVAL: Duration = Duration::from_secs(60);

        let mut connected = Vec::with_capacity(MAX_SENT_PEERS);
        let mut dropped = Vec::with_capacity(MAX_SENT_PEERS);
        let mut peer_view_of_live_peers = HashSet::new();

        // Wait 10 seconds before sending the first message to assure that peer will stay with us
        tokio::time::sleep(Duration::from_secs(10)).await;

        let mut interval = tokio::time::interval(PEX_MESSAGE_INTERVAL);

        loop {
            interval.tick().await;

            {
                let live_peers = self.peers.live_outgoing_peers.read();
                connected.clear();
                dropped.clear();

                connected.extend(
                    live_peers
                        .difference(&peer_view_of_live_peers)
                        .take(MAX_SENT_PEERS)
                        .copied(),
                );
                dropped.extend(
                    peer_view_of_live_peers
                        .difference(&live_peers)
                        .take(MAX_SENT_PEERS)
                        .copied(),
                );
            }

            // BEP 11 - Dont send closed if they are now in live
            // it's assured by mutual exclusion of two  above sets  if in sent_peers_live, it cannot be in addrs_live_to_sent,
            // and addrs_closed_to_sent are only filtered addresses from sent_peers_live

            if !connected.is_empty() || !dropped.is_empty() {
                let pex_msg = extended::ut_pex::UtPex::from_addrs(&connected, &dropped);
                let ext_msg = extended::ExtendedMessage::UtPex(pex_msg);
                if tx
                    .send(WriterRequest::Message(Message::Extended(ext_msg)))
                    .is_err()
                {
                    return Ok(()); // Peer disconnected
                }

                for addr in &dropped {
                    peer_view_of_live_peers.remove(addr);
                }
                peer_view_of_live_peers.extend(connected.iter().copied());
            }
        }
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
    incoming: bool,
    tx: PeerTx,

    first_message_received: AtomicBool,
}

impl PeerConnectionHandler for &PeerHandler {
    fn on_connected(&self, connection_time: Duration) {
        self.counters
            .outgoing_connections
            .fetch_add(1, Ordering::Relaxed);
        #[allow(clippy::cast_possible_truncation)]
        self.counters
            .total_time_connecting_ms
            .fetch_add(connection_time.as_millis() as u64, Ordering::Relaxed);
    }

    async fn on_received_message(&self, message: Message<ByteBuf<'_>>) -> anyhow::Result<()> {
        // The first message must be "bitfield", but if it's not sent,
        // assume the bitfield is all zeroes and was sent.
        if !matches!(&message, Message::Bitfield(..))
            && !self.first_message_received.swap(true, Ordering::Relaxed)
        {
            self.on_bitfield_notify.notify_waiters();
        }

        match message {
            Message::Request(request) => {
                self.on_download_request(request)
                    .context("on_download_request")?;
            }
            Message::Bitfield(b) => self
                .on_bitfield(b.clone_to_owned(None))
                .context("on_bitfield")?,
            Message::Choke => self.on_i_am_choked(),
            Message::Unchoke => self.on_i_am_unchoked(),
            Message::Interested => self.on_peer_interested(),
            Message::Piece(piece) => self
                .on_received_piece(piece)
                .await
                .context("on_received_piece")?,
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
            Message::Extended(ExtendedMessage::UtMetadata(UtMetadata::Request(
                metadata_piece_id,
            ))) => {
                self.send_metadata_piece(metadata_piece_id)
                    .with_context(|| format!("error sending metadata piece {metadata_piece_id}"))?;
            }
            Message::Extended(ExtendedMessage::UtPex(pex)) => {
                self.on_pex_message(pex);
            }
            message => {
                warn!("received unsupported message {:?}, ignoring", message);
            }
        };
        Ok(())
    }

    fn serialize_bitfield_message_to_buf(&self, buf: &mut Vec<u8>) -> anyhow::Result<usize> {
        let g = self.state.lock_read("serialize_bitfield_message_to_buf");
        let msg = Message::Bitfield(ByteBuf(g.get_chunks()?.get_have_pieces().as_bytes()));
        let len = msg.serialize(buf, &Default::default)?;
        trace!("sending: {:?}, length={}", &msg, len);
        Ok(len)
    }

    fn on_handshake<B>(&self, handshake: Handshake<B>) -> anyhow::Result<()> {
        self.state.set_peer_live(self.addr, handshake);
        Ok(())
    }

    fn on_uploaded_bytes(&self, bytes: u32) {
        self.state
            .stats
            .uploaded_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
        self.state
            .session_stats
            .uploaded_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    fn read_chunk(&self, chunk: &ChunkInfo, buf: &mut [u8]) -> anyhow::Result<()> {
        self.state.file_ops().read_chunk(self.addr, chunk, buf)
    }

    fn on_extended_handshake(&self, hs: &ExtendedHandshake<ByteBuf>) -> anyhow::Result<()> {
        if let Some(_peer_pex_msg_id) = hs.ut_pex() {
            self.state.clone().spawn(
                error_span!(
                    parent: self.state.shared.span.clone(),
                    "sending_pex_to_peer",
                    peer = self.addr.to_string()
                ),
                self.state
                    .clone()
                    .task_send_pex_to_peer(self.addr, self.tx.clone()),
            );
        }
        // Lets update outgoing Socket address for incoming connection
        if self.incoming {
            if let Some(port) = hs.port() {
                let peer_ip = hs.ip_addr().unwrap_or(self.addr.ip());
                let outgoing_addr = SocketAddr::new(peer_ip, port);
                self.state
                    .peers
                    .with_peer_mut(self.addr, "update outgoing addr", |peer| {
                        peer.outgoing_address = Some(outgoing_addr)
                    });
            }
        }
        Ok(())
    }

    fn should_send_bitfield(&self) -> bool {
        if self.state.torrent().options.disable_upload() {
            return false;
        }

        self.state.get_approx_have_bytes() > 0
    }

    fn should_transmit_have(&self, id: ValidPieceIndex) -> bool {
        if self.state.shared.options.disable_upload() {
            return false;
        }
        let have = self
            .state
            .peers
            .with_live(self.addr, |l| {
                l.bitfield.get(id.get_usize()).map(|p| *p).unwrap_or(true)
            })
            .unwrap_or(true);
        !have
    }

    fn update_my_extended_handshake(
        &self,
        handshake: &mut ExtendedHandshake<ByteBuf>,
    ) -> anyhow::Result<()> {
        let info_bytes = &self.state.metadata.info_bytes;
        if !info_bytes.is_empty() {
            if let Ok(len) = info_bytes.len().try_into() {
                handshake.metadata_size = Some(len);
            }
        }
        Ok(())
    }
}

impl PeerHandler {
    fn on_peer_died(self, error: Option<anyhow::Error>) -> anyhow::Result<()> {
        let peers = &self.state.peers;
        let handle = self.addr;
        let mut pe = match peers.states.get_mut(&handle) {
            Some(peer) => TimedExistence::new(peer, "on_peer_died"),
            None => {
                warn!("bug: peer not found in table. Forgetting it forever");
                return Ok(());
            }
        };
        let prev = pe.value_mut().take_state(peers);

        match prev {
            PeerState::Connecting(_) => {}
            PeerState::Live(live) => {
                let mut g = self.state.lock_write("mark_chunk_requests_canceled");
                for req in live.inflight_requests {
                    trace!(
                        "peer dead, marking chunk request cancelled, index={}, chunk={}",
                        req.piece_index.get(),
                        req.chunk_index
                    );
                    g.get_chunks_mut()?
                        .mark_piece_broken_if_not_have(req.piece_index);
                    self.state.new_pieces_notify.notify_waiters();
                }
            }
            PeerState::NotNeeded => {
                // Restore it as std::mem::take() replaced it above.
                pe.value_mut().set_state(PeerState::NotNeeded, peers);
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
                pe.value_mut().set_state(PeerState::NotNeeded, peers);
                return Ok(());
            }
        };

        self.counters.errors.fetch_add(1, Ordering::Relaxed);

        if self.state.is_finished_and_no_active_streams() {
            debug!("torrent finished, not re-queueing");
            pe.value_mut().set_state(PeerState::NotNeeded, peers);
            return Ok(());
        }

        pe.value_mut().set_state(PeerState::Dead, peers);

        if self.incoming {
            // do not retry incoming peers
            debug!(
                peer = handle.to_string(),
                "incoming peer died, not re-queueing"
            );
            return Ok(());
        }

        let backoff = pe.value_mut().stats.backoff.next_backoff();

        // Prevent deadlocks.
        drop(pe);

        if let Some(dur) = backoff {
            if cfg!(feature = "_disable_reconnect_test") {
                return Ok(());
            }
            self.state.clone().spawn(
                error_span!(
                    parent: self.state.shared.span.clone(),
                    "wait_for_peer",
                    peer = handle.to_string(),
                    duration = format!("{dur:?}")
                ),
                async move {
                    trace!("waiting to reconnect again");
                    tokio::time::sleep(dur).await;
                    trace!("finished waiting");
                    self.state
                        .peers
                        .with_peer_mut(handle, "dead_to_queued", |peer| {
                            match peer.get_state() {
                                PeerState::Dead => {
                                    peer.set_state(PeerState::Queued, &self.state.peers)
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
                    let chunk_tracker = g.get_chunks()?;
                    let priority_streamed_pieces = self
                        .state
                        .streams
                        .iter_next_pieces(&self.state.lengths)
                        .filter(|pid| {
                            !chunk_tracker.is_piece_have(*pid)
                                && !g.inflight_pieces.contains_key(pid)
                        });
                    let natural_order_pieces = chunk_tracker
                        .iter_queued_pieces(&g.file_priorities, &self.state.metadata.file_infos);
                    for n in priority_streamed_pieces.chain(natural_order_pieces) {
                        if bf.get(n.get() as usize).map(|v| *v) == Some(true) {
                            n_opt = Some(n);
                            break;
                        }
                    }

                    match n_opt {
                        Some(n_opt) => n_opt,
                        None => return Ok(None),
                    }
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
        let my_avg_time = self.counters.average_piece_download_time()?;

        let (stolen_idx, from_peer) = {
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
                // If the piece is locked and someone is actively writing to disk, don't steal it.
                if let Some(_g) = self.state.per_piece_locks[idx.get_usize()].try_write() {
                    debug!(
                        "will steal piece {} from {}: elapsed time {:?}, my avg piece time: {:?}",
                        idx, piece_req.peer, elapsed, my_avg_time
                    );
                    let old = piece_req.peer;
                    piece_req.peer = self.addr;
                    piece_req.started = Instant::now();
                    (*idx, old)
                } else {
                    debug!(?idx, ?piece_req, "attempted to steal but peer was writing");
                    return None;
                }
            } else {
                return None;
            }
        };

        // Send cancellations to old peer and bump counters.
        self.state.peers.on_steal(from_peer, self.addr, stolen_idx);

        Some(stolen_idx)
    }

    fn on_download_request(&self, request: Request) -> anyhow::Result<()> {
        if self.state.torrent().options.disable_upload() {
            anyhow::bail!("upload disabled, but peer requested a piece")
        }

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

        self.state
            .ratelimit_upload_tx
            .send((self.tx.clone(), chunk_info))?;
        Ok(())
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
                if let Some(true) = live
                    .bitfield
                    .get(..self.state.lengths.total_pieces() as usize)
                    .map(|s| s.all())
                {
                    debug!("peer has full torrent");
                }
            });
        self.on_bitfield_notify.notify_waiters();
    }

    fn on_bitfield(&self, bitfield: ByteBufOwned) -> anyhow::Result<()> {
        if bitfield.len() != self.state.lengths.piece_bitfield_bytes() {
            anyhow::bail!(
                "dropping peer as its bitfield has unexpected size. Got {}, expected {}",
                bitfield.len(),
                self.state.lengths.piece_bitfield_bytes(),
            );
        }
        let bf = BF::from_boxed_slice(bitfield.0.to_vec().into_boxed_slice());
        if let Some(true) = bf
            .get(..self.state.lengths.total_pieces() as usize)
            .map(|s| s.all())
        {
            debug!("peer has full torrent");
        }
        self.state.peers.update_bitfield(self.addr, bf);
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

    // The job of this is to request chunks and also to keep peer alive.
    // The moment this ends, the peer is disconnected.
    async fn task_peer_chunk_requester(&self) -> anyhow::Result<()> {
        let handle = self.addr;
        self.wait_for_bitfield().await;

        let mut update_interest = {
            let mut current = false;
            move |h: &PeerHandler, new_value: bool| -> anyhow::Result<()> {
                if new_value != current {
                    h.tx.send(if new_value {
                        WriterRequest::Message(MessageOwned::Interested)
                    } else {
                        WriterRequest::Message(MessageOwned::NotInterested)
                    })?;
                    current = new_value;
                }
                Ok(())
            }
        };

        loop {
            // If we have full torrent, we don't need to request more pieces.
            // However we might still need to seed them to the peer.
            if self.state.is_finished_and_no_active_streams() {
                update_interest(self, false)?;
                if !self.state.peers.is_peer_interested(self.addr) {
                    debug!("nothing left to do, neither of us is interested, disconnecting peer");
                    self.tx.send(WriterRequest::Disconnect(Ok(())))?;
                    // wait until the receiver gets the message so that it doesn't finish with an error.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    return Ok(());
                } else {
                    // TODO: wait for a notification of interest, e.g. update of selected files or new streams or change
                    // in peer interest.
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            }

            update_interest(self, true)?;
            aframe!(self.wait_for_unchoke()).await;

            // Try steal a pice from a very slow peer first. Otherwise we might wait too long
            // to download early pieces.
            // Then try get the next one in queue.
            // Afterwards means we are close to completion, try stealing more aggressively.
            let new_piece_notify = self.state.new_pieces_notify.notified();
            let next = match self
                .try_steal_old_slow_piece(10.)
                .map_or_else(|| self.reserve_next_needed_piece(), |v| Ok(Some(v)))?
                .or_else(|| self.try_steal_old_slow_piece(3.))
            {
                Some(next) => next,
                None => {
                    debug!("no pieces to request");
                    match aframe!(tokio::time::timeout(
                        // Half of default rw timeout not to race with it.
                        Duration::from_secs(5),
                        new_piece_notify
                    ))
                    .await
                    {
                        Ok(()) => debug!("woken up, new pieces might be available"),
                        Err(_) => debug!("woken up by sleep timer"),
                    }
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
                        live.inflight_requests.insert(chunk)
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

                self.state
                    .ratelimits
                    .prepare_for_download(NonZeroU32::new(request.length).unwrap())
                    .await?;

                if let Some(session) = self.state.torrent().session.upgrade() {
                    session
                        .ratelimits
                        .prepare_for_download(NonZeroU32::new(request.length).unwrap())
                        .await?;
                }

                loop {
                    match aframe!(tokio::time::timeout(
                        Duration::from_secs(5),
                        aframe!(self.requests_sem.acquire())
                    ))
                    .await
                    {
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

    fn on_i_am_unchoked(&self) {
        trace!("we are unchoked");
        self.locked.write().i_am_choked = false;
        self.unchoke_notify.notify_waiters();
        // 128 should be more than enough to maintain 100mbps
        // for a single peer that has 100ms ping
        // https://www.desmos.com/calculator/x3szur87ps
        self.requests_sem.add_permits(128);
    }

    async fn on_received_piece(&self, piece: Piece<ByteBuf<'_>>) -> anyhow::Result<()> {
        let piece_index = self
            .state
            .lengths
            .validate_piece_index(piece.index)
            .with_context(|| format!("peer sent an invalid piece {}", piece.index))?;
        let chunk_info = match self.state.lengths.chunk_info_from_received_data(
            piece_index,
            piece.begin,
            piece.block.len().try_into().context("bug")?,
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

        self.state
            .peers
            .with_live_mut(self.addr, "inflight_requests.remove", |h| {
                if !h.inflight_requests.remove(&chunk_info) {
                    anyhow::bail!(
                        "peer sent us a piece we did not ask. Requested pieces: {:?}. Got: {:?}",
                        &h.inflight_requests,
                        &piece,
                    );
                }
                Ok(())
            })
            .context("peer not found")??;

        // This one is used to calculate download speed.
        self.state
            .stats
            .fetched_bytes
            .fetch_add(piece.block.as_ref().len() as u64, Ordering::Relaxed);
        self.state
            .session_stats
            .fetched_bytes
            .fetch_add(piece.block.len() as u64, Ordering::Relaxed);

        fn write_to_disk(
            state: &TorrentStateLive,
            addr: PeerHandle,
            counters: &AtomicPeerCounters,
            piece: &Piece<impl AsRef<[u8]> + std::fmt::Debug>,
            chunk_info: &ChunkInfo,
        ) -> anyhow::Result<()> {
            let index = piece.index;

            // If someone stole the piece by now, ignore it.
            // However if they didn't, don't let them steal it while we are writing.
            // So that by the time we are done writing AND if it was the last piece,
            // we can actually checksum etc.
            // Otherwise it might get into some weird state.
            let ppl_guard = {
                let g = state.lock_read("check_steal");

                let ppl = state
                    .per_piece_locks
                    .get(piece.index as usize)
                    .map(|l| l.read());

                match g.inflight_pieces.get(&chunk_info.piece_index) {
                    Some(InflightPiece { peer, .. }) if *peer == addr => {}
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

                ppl
            };

            // While we hold per piece lock, noone can steal it.
            // So we can proceed writing knowing that the piece is ours now and will still be by the time
            // the write is finished.
            //

            if !cfg!(feature = "_disable_disk_write_net_benchmark") {
                match state.file_ops().write_chunk(addr, piece, chunk_info) {
                    Ok(()) => {}
                    Err(e) => {
                        error!("FATAL: error writing chunk to disk: {e:#}");
                        return state.on_fatal_error(e);
                    }
                };
            }

            let full_piece_download_time = {
                let mut g = state.lock_write("mark_chunk_downloaded");
                let chunk_marking_result = g.get_chunks_mut()?.mark_chunk_downloaded(piece);
                trace!(?piece, chunk_marking_result=?chunk_marking_result);

                match chunk_marking_result {
                    Some(ChunkMarkingResult::Completed) => {
                        trace!("piece={} done, will write and checksum", piece.index);
                        // This will prevent others from stealing it.
                        {
                            let piece = chunk_info.piece_index;
                            g.inflight_pieces.remove(&piece)
                        }
                        .map(|t| t.started.elapsed())
                    }
                    Some(ChunkMarkingResult::PreviouslyCompleted) => {
                        // TODO: we might need to send cancellations here.
                        debug!("piece={} was done by someone else, ignoring", piece.index);
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

            // We don't care about per piece lock anymore, as it's removed from inflight pieces.
            // It shouldn't impact perf anyway, but dropping just in case.
            drop(ppl_guard);

            let full_piece_download_time = match full_piece_download_time {
                Some(t) => t,
                None => return Ok(()),
            };

            match state
                .file_ops()
                .check_piece(chunk_info.piece_index)
                .with_context(|| format!("error checking piece={index}"))?
            {
                true => {
                    {
                        let mut g = state.lock_write("mark_piece_downloaded");
                        g.get_chunks_mut()?
                            .mark_piece_downloaded(chunk_info.piece_index);
                    }

                    // Global piece counters.
                    let piece_len = state.lengths.piece_length(chunk_info.piece_index) as u64;
                    state
                        .stats
                        .downloaded_and_checked_bytes
                        // This counter is used to compute "is_finished", so using
                        // stronger ordering.
                        .fetch_add(piece_len, Ordering::Release);
                    state
                        .stats
                        .downloaded_and_checked_pieces
                        // This counter is used to compute "is_finished", so using
                        // stronger ordering.
                        .fetch_add(1, Ordering::Release);
                    state
                        .stats
                        .have_bytes
                        .fetch_add(piece_len, Ordering::Relaxed);
                    #[allow(clippy::cast_possible_truncation)]
                    state.stats.total_piece_download_ms.fetch_add(
                        full_piece_download_time.as_millis() as u64,
                        Ordering::Relaxed,
                    );

                    // Per-peer piece counters.
                    counters.on_piece_completed(piece_len, full_piece_download_time);
                    state.peers.reset_peer_backoff(addr);

                    trace!(piece = index, "successfully downloaded and verified");

                    state.on_piece_completed(chunk_info.piece_index)?;

                    state.transmit_haves(chunk_info.piece_index);
                }
                false => {
                    warn!(
                        "checksum for piece={} did not validate. disconecting peer.",
                        index
                    );
                    state
                        .lock_write("mark_piece_broken")
                        .get_chunks_mut()?
                        .mark_piece_broken_if_not_have(chunk_info.piece_index);
                    state.new_pieces_notify.notify_waiters();
                    anyhow::bail!("i am probably a bogus peer. dying.")
                }
            };
            Ok(())
        }

        if let Some(dtx) = self.state.disk_work_tx() {
            // TODO: shove all this into one thing to .clone() once rather than 5 times.
            let state = self.state.clone();
            let addr = self.addr;
            let counters = self.counters.clone();
            let piece = piece.clone_to_owned(None);
            let tx = self.tx.clone();

            let span = tracing::error_span!("deferred_write");
            let work = move || {
                span.in_scope(|| {
                    if let Err(e) = write_to_disk(&state, addr, &counters, &piece, &chunk_info) {
                        let _ = tx.send(WriterRequest::Disconnect(Err(e)));
                    }
                })
            };
            dtx.send(Box::new(work)).await?;
        } else {
            self.state
                .shared
                .spawner
                .spawn_block_in_place(|| {
                    write_to_disk(&self.state, self.addr, &self.counters, &piece, &chunk_info)
                })
                .with_context(|| format!("error processing received chunk {chunk_info:?}"))?;
        }

        Ok(())
    }

    fn send_metadata_piece(&self, piece_id: u32) -> anyhow::Result<()> {
        let data = &self.state.metadata.info_bytes;
        let metadata_size = data.len();
        if metadata_size == 0 {
            anyhow::bail!("peer requested for info metadata but we don't have it")
        }
        let total_pieces: usize = (metadata_size as u64)
            .div_ceil(CHUNK_SIZE as u64)
            .try_into()?;

        if piece_id as usize > total_pieces {
            bail!("piece out of bounds")
        }

        let offset = piece_id * CHUNK_SIZE;
        let end = (offset + CHUNK_SIZE).min(data.len().try_into()?);
        let data = data.slice(offset as usize..end as usize);

        self.tx
            .send(WriterRequest::Message(Message::Extended(
                ExtendedMessage::UtMetadata(UtMetadata::Data {
                    piece: piece_id,
                    total_size: end - offset,
                    data: data.into(),
                }),
            )))
            .context("error sending UtMetadata: channel closed")?;
        Ok(())
    }

    fn on_pex_message<B>(&self, msg: UtPex<B>)
    where
        B: AsRef<[u8]> + std::fmt::Debug,
    {
        // TODO: this is just first attempt at pex - will need more sophistication on adding peers - BEP 40,  check number of live, seen peers ...
        msg.dropped_peers()
            .chain(msg.added_peers())
            .for_each(|peer| {
                self.state
                    .add_peer_if_not_seen(peer.addr)
                    .map_err(|error| {
                        warn!(?peer, ?error, "failed to add peer");
                        error
                    })
                    .ok();
            });
    }
}
