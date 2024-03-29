pub mod initializing;
pub mod live;
pub mod paused;
pub mod stats;
pub mod utils;

use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::bail;
use anyhow::Context;
use buffers::ByteString;
use futures::future::BoxFuture;
use futures::FutureExt;
use librqbit_core::hash_id::Id20;
use librqbit_core::lengths::Lengths;
use librqbit_core::peer_id::generate_peer_id;

use librqbit_core::spawn_utils::spawn_with_cancel;
use librqbit_core::torrent_metainfo::TorrentMetaV1Info;
pub use live::*;
use parking_lot::RwLock;

use tokio::time::timeout;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use tracing::error_span;
use tracing::warn;

use crate::chunk_tracker::ChunkTracker;
use crate::spawn_utils::BlockingSpawner;
use crate::torrent_state::stats::LiveStats;
use crate::type_aliases::PeerStream;

use initializing::TorrentStateInitializing;

use self::paused::TorrentStatePaused;
pub use self::stats::{TorrentStats, TorrentStatsState};

pub enum ManagedTorrentState {
    Initializing(Arc<TorrentStateInitializing>),
    Paused(TorrentStatePaused),
    Live(Arc<TorrentStateLive>),
    Error(anyhow::Error),

    // This is used when swapping between states, outside world should never see it.
    None,
}

impl ManagedTorrentState {
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
    pub state: ManagedTorrentState,
}

#[derive(Default)]
pub(crate) struct ManagedTorrentOptions {
    pub force_tracker_interval: Option<Duration>,
    pub peer_connect_timeout: Option<Duration>,
    pub peer_read_write_timeout: Option<Duration>,
    pub overwrite: bool,
}

pub struct ManagedTorrentInfo {
    pub info: TorrentMetaV1Info<ByteString>,
    pub info_hash: Id20,
    pub out_dir: PathBuf,
    pub(crate) spawner: BlockingSpawner,
    pub trackers: HashSet<String>,
    pub peer_id: Id20,
    pub lengths: Lengths,
    pub span: tracing::Span,
    pub(crate) options: ManagedTorrentOptions,
}

pub struct ManagedTorrent {
    pub info: Arc<ManagedTorrentInfo>,
    pub(crate) only_files: Option<Vec<usize>>,
    locked: RwLock<ManagedTorrentLocked>,
}

impl ManagedTorrent {
    pub fn info(&self) -> &ManagedTorrentInfo {
        &self.info
    }

    pub fn get_total_bytes(&self) -> u64 {
        self.info.lengths.total_length()
    }

    pub fn info_hash(&self) -> Id20 {
        self.info.info_hash
    }

    pub fn only_files(&self) -> Option<Vec<usize>> {
        self.only_files.clone()
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

        g.state = ManagedTorrentState::Error(error)
    }

    pub(crate) fn start(
        self: &Arc<Self>,
        peer_rx: Option<PeerStream>,
        start_paused: bool,
        live_cancellation_token: CancellationToken,
    ) -> anyhow::Result<()> {
        let mut g = self.locked.write();

        let spawn_fatal_errors_receiver =
            |state: &Arc<Self>,
             rx: tokio::sync::oneshot::Receiver<anyhow::Error>,
             token: CancellationToken| {
                let span = state.info.span.clone();
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
                error_span!(parent: live.meta().span.clone(), "external_peer_adder"),
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
                                    let live = match live.upgrade() {
                                        Some(live) => live,
                                        None => return Ok(()),
                                    };
                                    live.add_peer_if_not_seen(peer).context("torrent closed")?;
                                }
                                Ok(None) => return Ok(()),
                                // If timeout, check if the torrent is live.
                                Err(_) if live.strong_count() == 0 => return Ok(()),
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
                let span = self.info().span.clone();
                let token = live_cancellation_token.clone();
                spawn_with_cancel(
                    error_span!(parent: span.clone(), "initialize_and_start"),
                    token.clone(),
                    async move {
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
                                    return Ok(());
                                }

                                let (tx, rx) = tokio::sync::oneshot::channel();
                                let live =
                                    TorrentStateLive::new(paused, tx, live_cancellation_token);
                                g.state = ManagedTorrentState::Live(live.clone());

                                spawn_fatal_errors_receiver(&t, rx, token);
                                spawn_peer_adder(&live, peer_rx);

                                Ok(())
                            }
                            Err(err) => {
                                let result = anyhow::anyhow!("{:?}", err);
                                t.locked.write().state = ManagedTorrentState::Error(err);
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
                let live = TorrentStateLive::new(paused, tx, live_cancellation_token.clone());
                g.state = ManagedTorrentState::Live(live.clone());
                spawn_fatal_errors_receiver(self, rx, live_cancellation_token);
                spawn_peer_adder(&live, peer_rx);
                Ok(())
            }
            ManagedTorrentState::Error(_) => {
                let initializing = Arc::new(TorrentStateInitializing::new(
                    self.info.clone(),
                    self.only_files.clone(),
                ));
                g.state = ManagedTorrentState::Initializing(initializing.clone());
                drop(g);

                // Recurse.
                self.start(peer_rx, start_paused, live_cancellation_token)
            }
            ManagedTorrentState::None => bail!("bug: torrent is in empty state"),
        }
    }

    /// Pause the torrent if it's live.
    pub fn pause(&self) -> anyhow::Result<()> {
        let mut g = self.locked.write();
        match &g.state {
            ManagedTorrentState::Live(live) => {
                let paused = live.pause()?;
                g.state = ManagedTorrentState::Paused(paused);
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
            total_bytes: self.info().lengths.total_length(),
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
                    resp.total_bytes = p.chunk_tracker.get_total_selected_bytes();
                    resp.progress_bytes = resp.total_bytes - p.needed_bytes;
                    resp.finished = resp.progress_bytes == resp.total_bytes;
                }
                ManagedTorrentState::Live(l) => {
                    resp.state = S::Live;
                    let live_stats = LiveStats::from(l.as_ref());
                    let total = l.get_total_selected_bytes();
                    let remaining = l.get_left_to_download_bytes();
                    let progress = total - remaining;

                    resp.progress_bytes = progress;
                    resp.total_bytes = total;
                    resp.finished = remaining == 0;
                    resp.uploaded_bytes = l.get_uploaded_bytes();
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
                tokio::time::sleep(Duration::from_secs(1)).await;
            };

            live.wait_until_completed().await;
            Ok(())
        }
        .boxed()
    }
}

pub struct ManagedTorrentBuilder {
    info: TorrentMetaV1Info<ByteString>,
    info_hash: Id20,
    output_folder: PathBuf,
    force_tracker_interval: Option<Duration>,
    peer_connect_timeout: Option<Duration>,
    peer_read_write_timeout: Option<Duration>,
    only_files: Option<Vec<usize>>,
    trackers: Vec<String>,
    peer_id: Option<Id20>,
    overwrite: bool,
    spawner: Option<BlockingSpawner>,
}

impl ManagedTorrentBuilder {
    pub fn new<P: AsRef<Path>>(
        info: TorrentMetaV1Info<ByteString>,
        info_hash: Id20,
        output_folder: P,
    ) -> Self {
        Self {
            info,
            info_hash,
            output_folder: output_folder.as_ref().into(),
            spawner: None,
            force_tracker_interval: None,
            peer_connect_timeout: None,
            peer_read_write_timeout: None,
            only_files: None,
            trackers: Default::default(),
            peer_id: None,
            overwrite: false,
        }
    }

    pub fn only_files(&mut self, only_files: Vec<usize>) -> &mut Self {
        self.only_files = Some(only_files);
        self
    }

    pub fn trackers(&mut self, trackers: Vec<String>) -> &mut Self {
        self.trackers = trackers;
        self
    }

    pub fn overwrite(&mut self, overwrite: bool) -> &mut Self {
        self.overwrite = overwrite;
        self
    }

    pub fn force_tracker_interval(&mut self, force_tracker_interval: Duration) -> &mut Self {
        self.force_tracker_interval = Some(force_tracker_interval);
        self
    }

    pub(crate) fn spawner(&mut self, spawner: BlockingSpawner) -> &mut Self {
        self.spawner = Some(spawner);
        self
    }

    pub fn peer_id(&mut self, peer_id: Id20) -> &mut Self {
        self.peer_id = Some(peer_id);
        self
    }

    pub fn peer_connect_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.peer_connect_timeout = Some(timeout);
        self
    }

    pub fn peer_read_write_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.peer_read_write_timeout = Some(timeout);
        self
    }

    pub(crate) fn build(self, span: tracing::Span) -> anyhow::Result<ManagedTorrentHandle> {
        let lengths = Lengths::from_torrent(&self.info)?;
        let info = Arc::new(ManagedTorrentInfo {
            span,
            info: self.info,
            info_hash: self.info_hash,
            out_dir: self.output_folder,
            trackers: self.trackers.into_iter().collect(),
            spawner: self.spawner.unwrap_or_default(),
            peer_id: self.peer_id.unwrap_or_else(generate_peer_id),
            lengths,
            options: ManagedTorrentOptions {
                force_tracker_interval: self.force_tracker_interval,
                peer_connect_timeout: self.peer_connect_timeout,
                peer_read_write_timeout: self.peer_read_write_timeout,
                overwrite: self.overwrite,
            },
        });
        let initializing = Arc::new(TorrentStateInitializing::new(
            info.clone(),
            self.only_files.clone(),
        ));
        Ok(Arc::new(ManagedTorrent {
            only_files: self.only_files,
            locked: RwLock::new(ManagedTorrentLocked {
                state: ManagedTorrentState::Initializing(initializing),
            }),
            info,
        }))
    }
}

pub type ManagedTorrentHandle = Arc<ManagedTorrent>;
