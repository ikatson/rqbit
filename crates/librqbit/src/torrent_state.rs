// The main logic of rqbit is here - connecting to peers, reading and writing messages
// to them, tracking peer state etc.

// NOTE: deadlock notice:
// peers and stateLocked are behind 2 different locks.
// if you lock them in different order, this may deadlock.
// so always lock the peers one first, and unlock it before stateLocked is locked.

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
use dashmap::DashMap;
use futures::{stream::FuturesUnordered, StreamExt};
use librqbit_core::{
    id20::Id20,
    lengths::{ChunkInfo, Lengths, ValidPieceIndex},
    torrent_metainfo::TorrentMetaV1Info,
};
use parking_lot::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use peer_binary_protocol::{
    extended::handshake::ExtendedHandshake, Handshake, Message, MessageOwned, Piece, Request,
};
use serde::Serialize;
use sha1w::Sha1;
use tokio::{
    sync::{
        mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
        Notify, Semaphore,
    },
    time::timeout,
};
use tracing::{debug, info, span, trace, warn, Level};

use crate::{
    chunk_tracker::{ChunkMarkingResult, ChunkTracker},
    file_ops::FileOps,
    peer_connection::{
        PeerConnection, PeerConnectionHandler, PeerConnectionOptions, WriterRequest,
    },
    peer_state::{InflightRequest, LivePeerState, Peer, PeerRx, PeerState, PeerTx, SendMany},
    spawn_utils::{spawn, BlockingSpawner},
    type_aliases::{PeerHandle, BF},
};

pub struct InflightPiece {
    pub peer: PeerHandle,
    pub started: Instant,
}

#[derive(Default)]
pub struct PeerStates {
    states: DashMap<PeerHandle, Peer>,
}

#[derive(Debug, Default)]
pub struct AggregatePeerStats {
    pub queued: usize,
    pub connecting: usize,
    pub live: usize,
    pub seen: usize,
    pub dead: usize,
    pub fully_have_and_we_are_finished: usize,
}

impl PeerStates {
    pub fn stats(&self) -> AggregatePeerStats {
        // TODO: it would be better to store these as atomic not to lock needlessly.
        // However this would probably cause even more spaghetti.
        timeit("PeerStates::stats", || {
            self.states
                .iter()
                .fold(AggregatePeerStats::default(), |mut s, p| {
                    s.seen += 1;
                    match &p.value().state {
                        PeerState::Connecting(_) => s.connecting += 1,
                        PeerState::Live(_) => s.live += 1,
                        PeerState::Queued => s.queued += 1,
                        PeerState::Dead => s.dead += 1,
                        PeerState::NotNeeded => s.fully_have_and_we_are_finished += 1,
                    };
                    s
                })
        })
    }
    pub fn add_if_not_seen(&self, addr: SocketAddr) -> Option<PeerHandle> {
        use dashmap::mapref::entry::Entry;
        match self.states.entry(addr) {
            Entry::Occupied(_) => None,
            Entry::Vacant(vac) => {
                vac.insert(Default::default());
                Some(addr)
            }
        }
    }
    pub fn with_peer<R>(&self, addr: PeerHandle, f: impl FnOnce(&Peer) -> R) -> Option<R> {
        self.states.get(&addr).map(|e| f(e.value()))
    }

    pub fn with_peer_mut<R>(
        &self,
        addr: PeerHandle,
        reason: &'static str,
        f: impl FnOnce(&mut Peer) -> R,
    ) -> Option<R> {
        timeit(reason, || self.states.get_mut(&addr))
            .map(|e| f(TimedExistence::new(e, reason).value_mut()))
    }
    pub fn with_live<R>(&self, addr: PeerHandle, f: impl FnOnce(&LivePeerState) -> R) -> Option<R> {
        self.states.get(&addr).and_then(|e| match &e.value().state {
            PeerState::Live(l) => Some(f(l)),
            _ => None,
        })
    }
    pub fn with_live_mut<R>(
        &self,
        addr: PeerHandle,
        reason: &'static str,
        f: impl FnOnce(&mut LivePeerState) -> R,
    ) -> Option<R> {
        self.with_peer_mut(addr, reason, |peer| match &mut peer.state {
            PeerState::Live(l) => Some(f(l)),
            _ => None,
        })
        .flatten()
    }

    pub fn mark_peer_dead(&self, handle: PeerHandle) -> Option<Option<LivePeerState>> {
        self.with_peer_mut(handle, "mark_peer_dead", |peer| peer.state.to_dead())
            .flatten()
    }
    pub fn drop_peer(&self, handle: PeerHandle) -> Option<Peer> {
        self.states.remove(&handle).map(|r| r.1)
    }
    pub fn mark_i_am_choked(&self, handle: PeerHandle, is_choked: bool) -> Option<bool> {
        self.with_live_mut(handle, "mark_i_am_choked", |live| {
            let prev = live.i_am_choked;
            live.i_am_choked = is_choked;
            prev
        })
    }
    pub fn mark_peer_interested(&self, handle: PeerHandle, is_interested: bool) -> Option<bool> {
        self.with_live_mut(handle, "mark_peer_interested", |live| {
            let prev = live.peer_interested;
            live.peer_interested = is_interested;
            prev
        })
    }
    pub fn update_bitfield_from_vec(
        &self,
        handle: PeerHandle,
        bitfield: Vec<u8>,
    ) -> Option<Option<BF>> {
        self.with_live_mut(handle, "update_bitfield_from_vec", |live| {
            let bitfield = BF::from_vec(bitfield);
            let prev = live.bitfield.take();
            live.bitfield = Some(bitfield);
            prev
        })
    }
    pub fn mark_peer_connecting(&self, h: PeerHandle) -> anyhow::Result<PeerRx> {
        self.with_peer_mut(h, "mark_peer_connecting", |peer| {
            peer.state
                .queued_to_connecting()
                .context("invalid peer state")
        })
        .context("peer not found in states")?
    }

    pub fn clone_tx(&self, handle: PeerHandle) -> Option<PeerTx> {
        self.with_live(handle, |live| live.tx.clone())
    }

    fn reset_peer_backoff(&self, handle: PeerHandle) {
        self.with_peer_mut(handle, "reset_peer_backoff", |p| {
            p.stats.backoff.reset();
        });
    }

    fn mark_peer_not_needed(&self, handle: PeerHandle) -> Option<LivePeerState> {
        self.with_peer_mut(handle, "mark_peer_not_needed", |peer| {
            peer.state.to_not_needed()
        })
        .flatten()
    }
}

pub struct TorrentStateLocked {
    pub chunks: ChunkTracker,
    pub inflight_pieces: HashMap<ValidPieceIndex, InflightPiece>,
}

impl TorrentStateLocked {
    pub fn remove_inflight_piece(&mut self, piece: ValidPieceIndex) -> Option<InflightPiece> {
        self.inflight_pieces.remove(&piece)
    }
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
    pub dead_peers: u32,
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
    pub peer_read_write_timeout: Option<Duration>,
}

pub struct TorrentState {
    peers: PeerStates,
    info: TorrentMetaV1Info<ByteString>,
    locked: Arc<RwLock<TorrentStateLocked>>,
    files: Vec<Arc<Mutex<File>>>,
    filenames: Vec<PathBuf>,
    info_hash: Id20,
    peer_id: Id20,
    lengths: Lengths,
    needed: u64,
    have_plus_needed: u64,
    stats: AtomicStats,
    options: TorrentStateOptions,

    // Limits how many active (occupying network resources) peers there are at a moment in time.
    peer_semaphore: Semaphore,

    // The queue for peer manager to connect to them.
    peer_queue_tx: UnboundedSender<SocketAddr>,

    finished_notify: Notify,
}

#[cfg(not(feature = "timed_existence"))]
mod timed_existence {
    use std::ops::{Deref, DerefMut};

    pub struct TimedExistence<T>(T);

    impl<T> TimedExistence<T> {
        #[inline(always)]
        pub fn new(object: T, _reason: &'static str) -> Self {
            Self(object)
        }
    }

    impl<T> Deref for TimedExistence<T> {
        type Target = T;

        #[inline(always)]
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl<T> DerefMut for TimedExistence<T> {
        #[inline(always)]
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }

    #[inline(always)]
    pub fn timeit<R>(_n: impl std::fmt::Display, f: impl FnOnce() -> R) -> R {
        f()
    }
}

#[cfg(feature = "timed_existence")]
mod timed_existence {
    use std::ops::{Deref, DerefMut};
    use std::time::{Duration, Instant};
    use tracing::warn;

    const MAX: Duration = Duration::from_millis(5);

    // Prints if the object exists for too long.
    // This is used to track long-lived locks for debugging.
    pub struct TimedExistence<T> {
        object: T,
        reason: &'static str,
        started: Instant,
    }

    impl<T> TimedExistence<T> {
        pub fn new(object: T, reason: &'static str) -> Self {
            Self {
                object,
                reason,
                started: Instant::now(),
            }
        }
    }

    impl<T> Drop for TimedExistence<T> {
        fn drop(&mut self) {
            let elapsed = self.started.elapsed();
            let reason = self.reason;
            if elapsed > MAX {
                warn!("elapsed on lock {reason:?}: {elapsed:?}")
            }
        }
    }

    impl<T> Deref for TimedExistence<T> {
        type Target = T;

        fn deref(&self) -> &Self::Target {
            &self.object
        }
    }

    impl<T> DerefMut for TimedExistence<T> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.object
        }
    }

    pub fn timeit<R>(name: impl std::fmt::Display, f: impl FnOnce() -> R) -> R {
        let now = Instant::now();
        let r = f();
        let elapsed = now.elapsed();
        if elapsed > MAX {
            warn!("elapsed on \"{name:}\": {elapsed:?}")
        }
        r
    }
}

pub use timed_existence::{timeit, TimedExistence};

impl TorrentState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        info: TorrentMetaV1Info<ByteString>,
        info_hash: Id20,
        peer_id: Id20,
        files: Vec<Arc<Mutex<File>>>,
        filenames: Vec<PathBuf>,
        chunk_tracker: ChunkTracker,
        lengths: Lengths,
        have_bytes: u64,
        needed_bytes: u64,
        spawner: BlockingSpawner,
        options: Option<TorrentStateOptions>,
    ) -> Arc<Self> {
        let options = options.unwrap_or_default();
        let (peer_queue_tx, peer_queue_rx) = unbounded_channel();
        let state = Arc::new(TorrentState {
            info_hash,
            info,
            peer_id,
            peers: Default::default(),
            locked: Arc::new(RwLock::new(TorrentStateLocked {
                chunks: chunk_tracker,
                inflight_pieces: Default::default(),
            })),
            files,
            filenames,
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
            finished_notify: Notify::new(),
        });
        spawn(
            span!(Level::ERROR, "peer_adder"),
            state.clone().task_peer_adder(peer_queue_rx, spawner),
        );
        state
    }

    pub async fn task_manage_peer(
        self: Arc<Self>,
        addr: SocketAddr,
        spawner: BlockingSpawner,
    ) -> anyhow::Result<()> {
        let state = self;
        let rx = state.peers.mark_peer_connecting(addr)?;

        let handler = PeerHandler {
            addr,
            state: state.clone(),
            spawner,
        };
        let options = PeerConnectionOptions {
            connect_timeout: state.options.peer_connect_timeout,
            read_write_timeout: state.options.peer_read_write_timeout,
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

        let res = peer_connection.manage_peer(rx).await;
        let state = peer_connection.into_handler().state;
        state.peer_semaphore.add_permits(1);

        match res {
            // We disconnected the peer ourselves as we don't need it
            Ok(()) => {
                state.on_peer_died(addr, None);
            }
            Err(e) => {
                debug!("error managing peer: {:#}", e);
                state.on_peer_died(addr, Some(e));
            }
        }
        Ok::<_, anyhow::Error>(())
    }

    pub async fn task_peer_adder(
        self: Arc<Self>,
        mut peer_queue_rx: UnboundedReceiver<SocketAddr>,
        spawner: BlockingSpawner,
    ) -> anyhow::Result<()> {
        let state = self;
        loop {
            let addr = peer_queue_rx.recv().await.unwrap();
            if state.is_finished() {
                debug!("ignoring peer {} as we are finished", addr);
                state.peers.mark_peer_not_needed(addr);
                continue;
            }

            let permit = state.peer_semaphore.acquire().await.unwrap();
            permit.forget();
            spawn(
                span!(parent: None, Level::ERROR, "manage_peer", peer = addr.to_string()),
                state.clone().task_manage_peer(addr, spawner),
            );
        }
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
    pub fn lock_read(
        &self,
        reason: &'static str,
    ) -> TimedExistence<RwLockReadGuard<TorrentStateLocked>> {
        TimedExistence::new(timeit(reason, || self.locked.read()), reason)
    }
    pub fn lock_write(
        &self,
        reason: &'static str,
    ) -> TimedExistence<RwLockWriteGuard<TorrentStateLocked>> {
        TimedExistence::new(timeit(reason, || self.locked.write()), reason)
    }

    fn get_next_needed_piece(&self, peer_handle: PeerHandle) -> Option<ValidPieceIndex> {
        self.peers
            .with_live_mut(peer_handle, "l(get_next_needed_piece)", |live| {
                let g = self.lock_read("g(get_next_needed_piece)");
                let bf = live.bitfield.as_ref()?;
                for n in g.chunks.iter_needed_pieces() {
                    if bf.get(n).map(|v| *v) == Some(true) {
                        // in theory it should be safe without validation, but whatever.
                        return self.lengths.validate_piece_index(n as u32);
                    }
                }
                None
            })?
    }

    fn am_i_choked(&self, peer_handle: PeerHandle) -> Option<bool> {
        self.peers.with_live(peer_handle, |l| l.i_am_choked)
    }

    fn reserve_next_needed_piece(&self, peer_handle: PeerHandle) -> Option<ValidPieceIndex> {
        // TODO: locking one inside the other in different order results in deadlocks.
        self.peers
            .with_live_mut(peer_handle, "reserve_next_needed_piece", |live| {
                if live.i_am_choked {
                    debug!("we are choked, can't reserve next piece");
                    return None;
                }
                let mut g = self.lock_write("reserve_next_needed_piece");
                let n = {
                    let mut n_opt = None;
                    let bf = live.bitfield.as_ref()?;
                    for n in g.chunks.iter_needed_pieces() {
                        if bf.get(n).map(|v| *v) == Some(true) {
                            n_opt = Some(n);
                            break;
                        }
                    }

                    self.lengths.validate_piece_index(n_opt? as u32)?
                };
                g.inflight_pieces.insert(
                    n,
                    InflightPiece {
                        peer: peer_handle,
                        started: Instant::now(),
                    },
                );
                g.chunks.reserve_needed_piece(n);
                Some(n)
            })
            .flatten()
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

        let mut g = self.lock_write("try_steal_old_slow_piece");
        let (idx, elapsed, piece_req) = g
            .inflight_pieces
            .iter_mut()
            // don't steal from myself
            .filter(|(_, r)| r.peer != handle)
            .map(|(p, r)| (p, r.started.elapsed(), r))
            .max_by_key(|(_, e, _)| *e)?;

        // heuristic for "too slow peer"
        if elapsed > avg_time * 10 {
            debug!(
                "will steal piece {} from {}: elapsed time {:?}, avg piece time: {:?}",
                idx, piece_req.peer, elapsed, avg_time
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
        self.peers
            .with_live(handle, |live| {
                let g = self.lock_read("try_steal_piece");
                g.inflight_pieces
                    .keys()
                    .filter(|p| !live.inflight_requests.iter().any(|req| req.piece == **p))
                    .choose(&mut rng)
                    .copied()
            })
            .flatten()
    }

    fn set_peer_live(&self, handle: PeerHandle, h: Handshake) {
        let result = self.peers.with_peer_mut(handle, "set_peer_live", |p| {
            p.state.connecting_to_live(Id20(h.peer_id)).is_some()
        });
        match result {
            Some(true) => debug!("set peer to live"),
            Some(false) => debug!("can't set peer live, it was in wrong state"),
            None => debug!("can't set peer live, it disappeared"),
        }
    }

    fn on_peer_died(self: &Arc<Self>, handle: PeerHandle, error: Option<anyhow::Error>) {
        let mut pe = match self.peers.states.get_mut(&handle) {
            Some(peer) => TimedExistence::new(peer, "on_peer_died"),
            None => {
                warn!("bug: peer not found in table. Forgetting it forever");
                return;
            }
        };
        match std::mem::take(&mut pe.value_mut().state) {
            PeerState::Connecting(_) => {}
            PeerState::Live(live) => {
                let mut g = self.lock_write("mark_chunk_requests_canceled");
                for req in live.inflight_requests {
                    debug!(
                        "peer dead, marking chunk request cancelled, index={}, chunk={}",
                        req.piece.get(),
                        req.chunk
                    );
                    g.chunks.mark_chunk_request_cancelled(req.piece, req.chunk);
                }
            }
            PeerState::NotNeeded => {
                // Restore it as std::mem::take() replaced it above.
                pe.value_mut().state = PeerState::NotNeeded;
                return;
            }
            s @ PeerState::Queued | s @ PeerState::Dead => {
                warn!("bug: peer was in a wrong state {s:?}, ignoring it forever");
                // Prevent deadlocks.
                drop(pe);
                self.peers.drop_peer(handle);
                return;
            }
        };

        if error.is_none() {
            debug!("peer died without errors, not re-queueing");
            pe.value_mut().state = PeerState::NotNeeded;
            return;
        }

        if self.is_finished() {
            debug!("torrent finished, not re-queueing");
            pe.value_mut().state = PeerState::NotNeeded;
            return;
        }

        pe.value_mut().state = PeerState::Dead;
        let backoff = pe.value_mut().stats.backoff.next_backoff();

        // Prevent deadlocks.
        drop(pe);

        if let Some(dur) = backoff {
            let state = self.clone();
            spawn(
                span!(
                    parent: None,
                    Level::ERROR,
                    "wait_for_peer",
                    peer = handle.to_string(),
                    duration = format!("{dur:?}")
                ),
                async move {
                    tokio::time::sleep(dur).await;
                    state
                        .peers
                        .with_peer_mut(handle, "dead_to_queued", |peer| {
                            match &peer.state {
                                PeerState::Dead => peer.state = PeerState::Queued,
                                other => bail!(
                                    "peer is in unexpected state: {}. Expected dead",
                                    other.name()
                                ),
                            };
                            Ok(())
                        })
                        .context("bug: peer disappeared")??;
                    state.peer_queue_tx.send(handle)?;
                    Ok::<_, anyhow::Error>(())
                },
            );
        } else {
            debug!("dropping peer, backoff exhausted");
            self.peers.drop_peer(handle);
        }
    }

    pub fn get_uploaded(&self) -> u64 {
        self.stats.uploaded.load(Ordering::Relaxed)
    }
    pub fn get_downloaded(&self) -> u64 {
        self.stats.downloaded_and_checked.load(Ordering::Relaxed)
    }

    pub fn is_finished(&self) -> bool {
        self.get_left_to_download() == 0
    }

    pub fn get_left_to_download(&self) -> u64 {
        self.needed - self.get_downloaded()
    }

    fn maybe_transmit_haves(&self, index: ValidPieceIndex) {
        let mut futures = Vec::new();

        for pe in self.peers.states.iter() {
            match &pe.value().state {
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
        spawn(
            span!(
                Level::ERROR,
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

    pub fn add_peer_if_not_seen(self: &Arc<Self>, addr: SocketAddr) -> bool {
        match self.peers.add_if_not_seen(addr) {
            Some(handle) => handle,
            None => return false,
        };

        self.peer_queue_tx.send(addr).unwrap();
        true
    }

    pub fn stats_snapshot(&self) -> StatsSnapshot {
        use Ordering::*;
        let peer_stats = self.peers.stats();
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
            seen_peers: peer_stats.seen as u32,
            connecting_peers: peer_stats.connecting as u32,
            time: Instant::now(),
            initially_needed_bytes: self.needed,
            remaining_bytes: remaining,
            queued_peers: peer_stats.queued as u32,
            dead_peers: peer_stats.dead as u32,
            total_piece_download_ms: self.stats.total_piece_download_ms.load(Relaxed),
        }
    }

    pub async fn wait_until_completed(&self) {
        if self.is_finished() {
            return;
        }
        self.finished_notify.notified().await;
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
                    .context("on_download_request")?;
            }
            Message::Bitfield(b) => self
                .on_bitfield(self.addr, b.clone_to_owned())
                .context("on_bitfield")?,
            Message::Choke => self.on_i_am_choked(self.addr),
            Message::Unchoke => self.on_i_am_unchoked(self.addr),
            Message::Interested => self.on_peer_interested(self.addr),
            Message::Piece(piece) => self
                .on_received_piece(self.addr, piece)
                .context("on_received_piece")?,
            Message::KeepAlive => {
                debug!("keepalive received");
            }
            Message::Have(h) => self.on_have(self.addr, h),
            Message::NotInterested => {
                info!("received \"not interested\", but we don't care yet")
            }
            message => {
                warn!("received unsupported message {:?}, ignoring", message);
            }
        };
        Ok(())
    }

    fn get_have_bytes(&self) -> u64 {
        self.state.stats.have.load(Ordering::Relaxed)
    }

    fn serialize_bitfield_message_to_buf(&self, buf: &mut Vec<u8>) -> Option<usize> {
        let g = self.state.lock_read("serialize_bitfield_message_to_buf");
        let msg = Message::Bitfield(ByteBuf(g.chunks.get_have_pieces().as_raw_slice()));
        let len = msg.serialize(buf, None).unwrap();
        debug!("sending: {:?}, length={}", &msg, len);
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
    #[inline(never)]
    fn on_download_request(&self, peer_handle: PeerHandle, request: Request) -> anyhow::Result<()> {
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

        let tx = {
            if !self
                .state
                .lock_read("is_chunk_ready_to_upload")
                .chunks
                .is_chunk_ready_to_upload(&chunk_info)
            {
                anyhow::bail!(
                    "got request for a chunk that is not ready to upload. chunk {:?}",
                    &chunk_info
                );
            }

            self.state
                .peers
                .clone_tx(peer_handle)
                .context("peer died, dropping chunk that it requested")?
        };

        // TODO: this is not super efficient as it does copying multiple times.
        // Theoretically, this could be done in the sending code, so that it reads straight into
        // the send buffer.
        let request = WriterRequest::ReadChunkRequest(chunk_info);
        debug!("sending {:?}", &request);
        Ok::<_, anyhow::Error>(tx.send(request)?)
    }

    #[inline(never)]
    fn on_have(&self, handle: PeerHandle, have: u32) {
        self.state.peers.with_live_mut(handle, "on_have", |live| {
            if let Some(bitfield) = live.bitfield.as_mut() {
                bitfield.set(have as usize, true);
                debug!("updated bitfield with have={}", have);
            }
        });
    }

    #[inline(never)]
    fn on_bitfield(&self, handle: PeerHandle, bitfield: ByteString) -> anyhow::Result<()> {
        if bitfield.len() != self.state.lengths.piece_bitfield_bytes() {
            anyhow::bail!(
                "dropping peer as its bitfield has unexpected size. Got {}, expected {}",
                bitfield.len(),
                self.state.lengths.piece_bitfield_bytes(),
            );
        }
        self.state
            .peers
            .update_bitfield_from_vec(handle, bitfield.0);

        if !self.state.am_i_interested_in_peer(handle) {
            let tx = self.state.peers.clone_tx(handle).context("peer dropped")?;
            tx.send(WriterRequest::Message(MessageOwned::Unchoke))?;
            tx.send(WriterRequest::Message(MessageOwned::NotInterested))?;
            if self.state.is_finished() {
                tx.send(WriterRequest::Disconnect)?;
            }
            return Ok(());
        }

        // Additional spawn per peer, not good.
        spawn(
            span!(
                Level::ERROR,
                "peer_chunk_requester",
                peer = handle.to_string()
            ),
            self.clone().task_peer_chunk_requester(handle),
        );
        Ok(())
    }

    async fn task_peer_chunk_requester(self, handle: PeerHandle) -> anyhow::Result<()> {
        let tx = match self.state.peers.clone_tx(handle) {
            Some(tx) => tx,
            None => return Ok(()),
        };
        tx.send_many([
            WriterRequest::Message(MessageOwned::Unchoke),
            WriterRequest::Message(MessageOwned::Interested),
        ])?;
        self.requester(handle).await?;
        Ok::<_, anyhow::Error>(())
    }

    #[inline(never)]
    fn on_i_am_choked(&self, handle: PeerHandle) {
        debug!("we are choked");
        self.state.peers.mark_i_am_choked(handle, true);
    }

    #[inline(never)]
    fn on_peer_interested(&self, handle: PeerHandle) {
        debug!("peer is interested");
        self.state.peers.mark_peer_interested(handle, true);
    }

    async fn requester(self, handle: PeerHandle) -> anyhow::Result<()> {
        let notify = match self
            .state
            .peers
            .with_live(handle, |l| l.have_notify.clone())
        {
            Some(notify) => notify,
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
                    debug!("we are choked, can't reserve next piece");
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
                            debug!("nothing left to download, closing requester");
                            return Ok(());
                        }

                        if let Some(piece) = self.state.try_steal_piece(handle) {
                            debug!("stole a piece {}", piece);
                            piece
                        } else {
                            debug!("no pieces to request");
                            #[allow(unused_must_use)]
                            {
                                timeout(Duration::from_secs(60), notify.notified()).await;
                            }
                            continue;
                        }
                    }
                },
            };

            let (tx, sem) = match self
                .state
                .peers
                .with_live(handle, |l| (l.tx.clone(), l.requests_sem.clone()))
            {
                Some((tx, sem)) => (tx, sem),
                None => return Ok(()),
            };

            for chunk in self.state.lengths.iter_chunk_infos(next) {
                if self
                    .state
                    .lock_read("is_chunk_downloaded")
                    .chunks
                    .is_chunk_downloaded(&chunk)
                {
                    continue;
                }

                match self
                    .state
                    .peers
                    .with_live_mut(handle, "inflight_requests.insert", |l| {
                        l.inflight_requests.insert(InflightRequest::from(&chunk))
                    }) {
                    Some(true) => {}
                    Some(false) => {
                        warn!("probably a bug, we already requested {:?}", chunk);
                        continue;
                    }
                    None => bail!("peer dropped"),
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

    fn reopen_read_only(&self) -> anyhow::Result<()> {
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
        Ok(())
    }

    #[inline(never)]
    fn on_i_am_unchoked(&self, handle: PeerHandle) {
        debug!("we are unchoked");
        self.state
            .peers
            .with_live_mut(handle, "on_i_am_unchoked", |live| {
                live.i_am_choked = false;
                live.have_notify.notify_waiters();
                live.requests_sem.add_permits(16);
            });
    }

    #[inline(never)]
    fn on_received_piece(&self, handle: PeerHandle, piece: Piece<ByteBuf>) -> anyhow::Result<()> {
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

        self.state
            .peers
            .with_live_mut(handle, "inflight_requests.remove", |h| {
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

            match g.chunks.mark_chunk_downloaded(&piece) {
                Some(ChunkMarkingResult::Completed) => {
                    debug!("piece={} done, will write and checksum", piece.index,);
                    // This will prevent others from stealing it.
                    g.remove_inflight_piece(chunk_info.piece_index)
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
                    .with_context(|| format!("error checking piece={index}"))?
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
                        {
                            let mut g = self.state.lock_write("mark_piece_downloaded");

                            g.chunks.mark_piece_downloaded(chunk_info.piece_index);
                            self.state.peers.reset_peer_backoff(handle);
                        }

                        debug!("piece={} successfully downloaded and verified", index);

                        if self.state.is_finished() {
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
                            .chunks
                            .mark_piece_broken(chunk_info.piece_index);
                    }
                };
                Ok::<_, anyhow::Error>(())
            })
            .with_context(|| format!("error processing received chunk {chunk_info:?}"))?;
        Ok(())
    }

    fn disconnect_all_peers_that_have_full_torrent(&self) {
        for mut pe in self.state.peers.states.iter_mut() {
            if let PeerState::Live(l) = &pe.value().state {
                if l.has_full_torrent(self.state.lengths.total_pieces() as usize) {
                    let live = pe.value_mut().state.to_not_needed().unwrap();
                    let _ = live.tx.send(WriterRequest::Disconnect);
                }
            }
        }
    }
}
