pub mod initializing;
pub mod live;
pub mod paused;
pub mod stats;
mod streaming;
pub mod utils;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;

use anyhow::bail;
use anyhow::Context;
use buffers::ByteBufOwned;
use bytes::Bytes;
use futures::future::BoxFuture;
use futures::FutureExt;
use librqbit_core::hash_id::Id20;
use librqbit_core::lengths::Lengths;

use librqbit_core::spawn_utils::spawn_with_cancel;
use librqbit_core::torrent_metainfo::TorrentMetaV1Info;
pub use live::*;
use parking_lot::RwLock;

use tokio::sync::Notify;
use tokio::time::timeout;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use tracing::error_span;
use tracing::warn;

use crate::chunk_tracker::ChunkTracker;
use crate::session::TorrentId;
use crate::spawn_utils::BlockingSpawner;
use crate::storage::BoxStorageFactory;
use crate::stream_connect::StreamConnector;
use crate::torrent_state::stats::LiveStats;
use crate::type_aliases::DiskWorkQueueSender;
use crate::type_aliases::FileInfos;
use crate::type_aliases::PeerStream;
use crate::Session;

use initializing::TorrentStateInitializing;

use self::paused::TorrentStatePaused;
pub use self::stats::{TorrentStats, TorrentStatsState};
pub use self::streaming::FileStream;

pub enum ManagedTorrentState {
    Initializing(Arc<TorrentStateInitializing>),
    Paused(TorrentStatePaused),
    Live(Arc<TorrentStateLive>),
    Error(anyhow::Error),

    // This is used when swapping between states, outside world should never see it.
    None,
}

impl ManagedTorrentState {
    pub fn name(&self) -> &'static str {
        match self {
            ManagedTorrentState::Initializing(_) => "initializing",
            ManagedTorrentState::Paused(_) => "paused",
            ManagedTorrentState::Live(_) => "live",
            ManagedTorrentState::Error(_) => "error",
            ManagedTorrentState::None => "<invalid: none>",
        }
    }

    fn assert_paused(self) -> TorrentStatePaused {
        match self {
            Self::Paused(paused) => paused,
            _ => panic!("Expected paused state"),
        }
    }

    pub(crate) fn take(&mut self) -> Self {
        std::mem::replace(self, Self::None)
    }
}

pub(crate) struct ManagedTorrentLocked {
    // The torrent might not be in "paused" state technically,
    // but the intention might be for it to stay paused.
    //
    // This should change only on "unpause".
    pub(crate) paused: bool,
    pub(crate) state: ManagedTorrentState,
    pub(crate) only_files: Option<Vec<usize>>,
}

#[derive(Default)]
pub(crate) struct ManagedTorrentOptions {
    pub force_tracker_interval: Option<Duration>,
    pub peer_connect_timeout: Option<Duration>,
    pub peer_read_write_timeout: Option<Duration>,
    pub allow_overwrite: bool,
    pub output_folder: PathBuf,
    pub disk_write_queue: Option<DiskWorkQueueSender>,
}

/// Common information about torrent shared among all possible states.
///
// The reason it's not inlined into ManagedTorrent is to break the Arc cycle:
// ManagedTorrent contains the current torrent state, which in turn needs access to a bunch
// of stuff, but it shouldn't access the state.
pub struct ManagedTorrentShared {
    pub id: TorrentId,
    pub info: TorrentMetaV1Info<ByteBufOwned>,
    pub torrent_bytes: Bytes,
    pub info_bytes: Bytes,
    pub info_hash: Id20,
    pub(crate) spawner: BlockingSpawner,
    pub trackers: HashSet<String>,
    pub peer_id: Id20,
    pub lengths: Lengths,
    pub file_infos: FileInfos,
    pub span: tracing::Span,
    pub(crate) options: ManagedTorrentOptions,
    pub(crate) connector: Arc<StreamConnector>,
    pub(crate) storage_factory: BoxStorageFactory,
    pub(crate) session: Weak<Session>,
}

pub struct ManagedTorrent {
    pub shared: Arc<ManagedTorrentShared>,
    pub(crate) state_change_notify: Notify,
    pub(crate) locked: RwLock<ManagedTorrentLocked>,
}

impl ManagedTorrent {
    pub fn id(&self) -> TorrentId {
        self.shared.id
    }

    pub fn shared(&self) -> &ManagedTorrentShared {
        &self.shared
    }

    pub fn get_total_bytes(&self) -> u64 {
        self.shared.lengths.total_length()
    }

    pub fn info_hash(&self) -> Id20 {
        self.shared.info_hash
    }

    pub fn only_files(&self) -> Option<Vec<usize>> {
        self.locked.read().only_files.clone()
    }

    pub fn with_state<R>(&self, f: impl FnOnce(&ManagedTorrentState) -> R) -> R {
        f(&self.locked.read().state)
    }

    pub(crate) fn with_state_mut<R>(&self, f: impl FnOnce(&mut ManagedTorrentState) -> R) -> R {
        f(&mut self.locked.write().state)
    }

    pub(crate) fn with_chunk_tracker<R>(
        &self,
        f: impl FnOnce(&ChunkTracker) -> R,
    ) -> anyhow::Result<R> {
        let g = self.locked.read();
        match &g.state {
            ManagedTorrentState::Paused(p) => Ok(f(&p.chunk_tracker)),
            ManagedTorrentState::Live(l) => Ok(f(l
                .lock_read("chunk_tracker")
                .get_chunks()
                .context("error getting chunks")?)),
            _ => bail!("no chunk tracker, torrent neither paused nor live"),
        }
    }

    /// Get the live state if the torrent is live.
    pub fn live(&self) -> Option<Arc<TorrentStateLive>> {
        let g = self.locked.read();
        match &g.state {
            ManagedTorrentState::Live(live) => Some(live.clone()),
            _ => None,
        }
    }

    fn stop_with_error(&self, error: anyhow::Error) {
        let mut g = self.locked.write();

        match g.state.take() {
            ManagedTorrentState::Live(live) => {
                if let Err(err) = live.pause() {
                    warn!(
                        "error pausing live torrent during fatal error handling: {:?}",
                        err
                    );
                }
            }
            ManagedTorrentState::Error(e) => {
                warn!("bug: torrent already was in error state when trying to stop it. Previous error was: {:?}", e);
            }
            ManagedTorrentState::None => {
                warn!("bug: torrent encountered in None state during fatal error handling")
            }
            _ => {}
        };

        self.state_change_notify.notify_waiters();

        g.state = ManagedTorrentState::Error(error)
    }

    pub(crate) fn start(
        self: &Arc<Self>,
        peer_rx: Option<PeerStream>,
        start_paused: bool,
    ) -> anyhow::Result<()> {
        let session = self
            .shared
            .session
            .upgrade()
            .context("session is dead, cannot start torrent")?;
        let mut g = self.locked.write();
        g.paused = start_paused;
        let cancellation_token = session.cancellation_token().child_token();

        let spawn_fatal_errors_receiver =
            |state: &Arc<Self>,
             rx: tokio::sync::oneshot::Receiver<anyhow::Error>,
             token: CancellationToken| {
                let span = state.shared.span.clone();
                let state = Arc::downgrade(state);
                spawn_with_cancel(
                    error_span!(parent: span, "fatal_errors_receiver"),
                    token,
                    async move {
                        let e = match rx.await {
                            Ok(e) => e,
                            Err(_) => return Ok(()),
                        };
                        if let Some(state) = state.upgrade() {
                            state.stop_with_error(e);
                        } else {
                            warn!("tried to stop the torrent with error, but couldn't upgrade the arc");
                        }
                        Ok(())
                    },
                );
            };

        fn spawn_peer_adder(live: &Arc<TorrentStateLive>, peer_rx: Option<PeerStream>) {
            live.spawn(
                error_span!(parent: live.torrent().span.clone(), "external_peer_adder"),
                {
                    let live = live.clone();
                    async move {
                        let live = {
                            let weak = Arc::downgrade(&live);
                            drop(live);
                            weak
                        };

                        let mut peer_rx = if let Some(peer_rx) = peer_rx {
                            peer_rx
                        } else {
                            return Ok(());
                        };

                        loop {
                            match timeout(Duration::from_secs(5), peer_rx.next()).await {
                                Ok(Some(peer)) => {
                                    debug!(?peer, "received peer from peer_rx");
                                    let live = match live.upgrade() {
                                        Some(live) => live,
                                        None => return Ok(()),
                                    };
                                    live.add_peer_if_not_seen(peer).context("torrent closed")?;
                                }
                                Ok(None) => {
                                    debug!("peer_rx closed, closing peer adder");
                                    return Ok(());
                                }
                                // If timeout, check if the torrent is live.
                                Err(_) if live.strong_count() == 0 => {
                                    debug!("timed out waiting for peers, torrent isn't live, closing peer adder");
                                    return Ok(());
                                }
                                Err(_) => continue,
                            }
                        }
                    }
                },
            );
        }

        match &g.state {
            ManagedTorrentState::Live(_) => {
                bail!("torrent is already live");
            }
            ManagedTorrentState::Initializing(init) => {
                let init = init.clone();
                drop(g);
                let t = self.clone();
                let span = self.shared().span.clone();
                let token = cancellation_token.clone();

                spawn_with_cancel(
                    error_span!(parent: span.clone(), "initialize_and_start"),
                    token.clone(),
                    async move {
                        let concurrent_init_semaphore =
                            session.concurrent_initialize_semaphore.clone();
                        let _permit = concurrent_init_semaphore
                            .acquire()
                            .await
                            .context("bug: concurrent init semaphore was closed")?;

                        match init.check().await {
                            Ok(paused) => {
                                let mut g = t.locked.write();
                                if let ManagedTorrentState::Initializing(_) = &g.state {
                                } else {
                                    debug!("no need to start torrent anymore, as it switched state from initilizing");
                                    return Ok(());
                                }

                                if start_paused {
                                    g.state = ManagedTorrentState::Paused(paused);
                                    t.state_change_notify.notify_waiters();
                                    return Ok(());
                                }

                                let (tx, rx) = tokio::sync::oneshot::channel();
                                let live = TorrentStateLive::new(paused, tx, cancellation_token)?;
                                g.state = ManagedTorrentState::Live(live.clone());
                                drop(g);

                                t.state_change_notify.notify_waiters();

                                spawn_fatal_errors_receiver(&t, rx, token);
                                spawn_peer_adder(&live, peer_rx);

                                Ok(())
                            }
                            Err(err) => {
                                let result = anyhow::anyhow!("{:?}", err);
                                t.locked.write().state = ManagedTorrentState::Error(err);
                                t.state_change_notify.notify_waiters();
                                Err(result)
                            }
                        }
                    },
                );
                Ok(())
            }
            ManagedTorrentState::Paused(_) => {
                let paused = g.state.take().assert_paused();
                let (tx, rx) = tokio::sync::oneshot::channel();
                let live = TorrentStateLive::new(paused, tx, cancellation_token.clone())?;
                g.state = ManagedTorrentState::Live(live.clone());
                drop(g);

                spawn_fatal_errors_receiver(self, rx, cancellation_token);
                spawn_peer_adder(&live, peer_rx);
                Ok(())
            }
            ManagedTorrentState::Error(_) => {
                let initializing = Arc::new(TorrentStateInitializing::new(
                    self.shared.clone(),
                    g.only_files.clone(),
                    self.shared.storage_factory.create_and_init(self.shared())?,
                    true,
                ));
                g.state = ManagedTorrentState::Initializing(initializing.clone());
                drop(g);

                self.state_change_notify.notify_waiters();

                // Recurse.
                self.start(peer_rx, start_paused)
            }
            ManagedTorrentState::None => bail!("bug: torrent is in empty state"),
        }
    }

    pub fn is_paused(&self) -> bool {
        self.locked.read().paused
    }

    /// Pause the torrent if it's live.
    pub(crate) fn pause(&self) -> anyhow::Result<()> {
        let mut g = self.locked.write();
        match &g.state {
            ManagedTorrentState::Live(live) => {
                let paused = live.pause()?;
                g.state = ManagedTorrentState::Paused(paused);
                self.state_change_notify.notify_waiters();
                Ok(())
            }
            ManagedTorrentState::Initializing(_) => {
                bail!("torrent is initializing, can't pause");
            }
            ManagedTorrentState::Paused(_) => {
                bail!("torrent is already paused");
            }
            ManagedTorrentState::Error(_) => {
                bail!("can't pause torrent in error state")
            }
            ManagedTorrentState::None => bail!("bug: torrent is in empty state"),
        }
    }

    /// Get stats.
    pub fn stats(&self) -> TorrentStats {
        use stats::TorrentStatsState as S;
        let mut resp = TorrentStats {
            total_bytes: self.shared().lengths.total_length(),
            file_progress: Vec::new(),
            state: S::Error,
            error: None,
            progress_bytes: 0,
            uploaded_bytes: 0,
            finished: false,
            live: None,
        };

        self.with_state(|s| {
            match s {
                ManagedTorrentState::Initializing(i) => {
                    resp.state = S::Initializing;
                    resp.progress_bytes = i.checked_bytes.load(Ordering::Relaxed);
                }
                ManagedTorrentState::Paused(p) => {
                    resp.state = S::Paused;
                    let hns = p.hns();
                    resp.total_bytes = hns.total();
                    resp.progress_bytes = hns.progress();
                    resp.finished = hns.finished();
                    resp.file_progress = p.chunk_tracker.per_file_have_bytes().to_owned();
                }
                ManagedTorrentState::Live(l) => {
                    resp.state = S::Live;
                    let live_stats = LiveStats::from(l.as_ref());
                    let hns = l.get_hns().unwrap_or_default();
                    resp.total_bytes = hns.total();
                    resp.progress_bytes = hns.progress();
                    resp.finished = hns.finished();
                    resp.uploaded_bytes = l.get_uploaded_bytes();
                    resp.file_progress = l
                        .lock_read("file_progress")
                        .get_chunks()
                        .ok()
                        .map(|c| c.per_file_have_bytes().to_owned())
                        .unwrap_or_default();
                    resp.live = Some(live_stats);
                }
                ManagedTorrentState::Error(e) => {
                    resp.state = S::Error;
                    resp.error = Some(format!("{:?}", e))
                }
                ManagedTorrentState::None => {
                    resp.state = S::Error;
                    resp.error = Some("bug: torrent in broken \"None\" state".to_string());
                }
            }
            resp
        })
    }

    #[inline(never)]
    pub fn wait_until_initialized(&self) -> BoxFuture<'_, anyhow::Result<()>> {
        async move {
            // TODO: rewrite, this polling is horrible
            loop {
                let done = self.with_state(|s| match s {
                    ManagedTorrentState::Initializing(_) => Ok(false),
                    ManagedTorrentState::Error(e) => bail!("{:?}", e),
                    ManagedTorrentState::None => bail!("bug: torrent state is None"),
                    _ => Ok(true),
                })?;
                if done {
                    return Ok(());
                }
                let _ = timeout(Duration::from_secs(1), self.state_change_notify.notified()).await;
            }
        }
        .boxed()
    }

    #[inline(never)]
    pub fn wait_until_completed(&self) -> BoxFuture<'_, anyhow::Result<()>> {
        async move {
            // TODO: rewrite, this polling is horrible
            let live = loop {
                let live = self.with_state(|s| match s {
                    ManagedTorrentState::Initializing(_) | ManagedTorrentState::Paused(_) => {
                        Ok(None)
                    }
                    ManagedTorrentState::Live(l) => Ok(Some(l.clone())),
                    ManagedTorrentState::Error(e) => bail!("{:?}", e),
                    ManagedTorrentState::None => bail!("bug: torrent state is None"),
                })?;
                if let Some(live) = live {
                    break live;
                }
                let _ = timeout(Duration::from_secs(1), self.state_change_notify.notified()).await;
            };

            live.wait_until_completed().await;
            Ok(())
        }
        .boxed()
    }

    // Returns true if needed to unpause torrent.
    // This is just implementation detail - it's easier to pause/unpause than to tinker with internals.
    pub(crate) fn update_only_files(&self, only_files: &HashSet<usize>) -> anyhow::Result<()> {
        let file_count = self.shared().info.iter_file_lengths()?.count();
        for f in only_files.iter().copied() {
            if f >= file_count {
                anyhow::bail!("only_files contains invalid value {f}")
            }
        }

        // if live, need to update chunk tracker
        // - if already finished: need to pause, then unpause (to reopen files etc)
        // if paused, need to update chunk tracker

        let mut g = self.locked.write();
        match &mut g.state {
            ManagedTorrentState::Initializing(_) => bail!("can't update initializing torrent"),
            ManagedTorrentState::Error(_) => {}
            ManagedTorrentState::None => {}
            ManagedTorrentState::Paused(p) => {
                p.update_only_files(only_files)?;
            }
            ManagedTorrentState::Live(l) => {
                l.update_only_files(only_files)?;
            }
        };

        g.only_files = Some(only_files.iter().copied().collect());
        Ok(())
    }
}

pub type ManagedTorrentHandle = Arc<ManagedTorrent>;
