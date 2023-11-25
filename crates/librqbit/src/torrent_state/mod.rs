pub mod initializing;
pub mod live;
pub mod paused;
pub mod utils;

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;

use anyhow::bail;
use anyhow::Context;
use buffers::ByteString;
use librqbit_core::id20::Id20;
use librqbit_core::lengths::Lengths;
use librqbit_core::peer_id::generate_peer_id;

use librqbit_core::torrent_metainfo::TorrentMetaV1Info;
pub use live::*;
use parking_lot::RwLock;

use tokio_stream::StreamExt;
use tracing::debug;
use tracing::error;
use tracing::error_span;
use url::Url;

use crate::chunk_tracker::ChunkTracker;
use crate::spawn_utils::spawn;
use crate::spawn_utils::BlockingSpawner;

use initializing::TorrentStateInitializing;

use self::paused::TorrentStatePaused;

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
    pub spawner: BlockingSpawner,
    pub trackers: HashSet<Url>,
    pub peer_id: Id20,
    pub lengths: Lengths,
    pub span: tracing::Span,
    pub(crate) options: ManagedTorrentOptions,
}

pub struct ManagedTorrent {
    pub info: Arc<ManagedTorrentInfo>,
    only_files: Option<Vec<usize>>,
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

    pub fn with_chunk_tracker<R>(&self, f: impl FnOnce(&ChunkTracker) -> R) -> anyhow::Result<R> {
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

    pub fn live(&self) -> Option<Arc<TorrentStateLive>> {
        let g = self.locked.read();
        match &g.state {
            ManagedTorrentState::Live(live) => Some(live.clone()),
            _ => None,
        }
    }

    pub fn start(
        self: &Arc<Self>,
        initial_peers: Vec<SocketAddr>,
        peer_rx: Option<impl StreamExt<Item = SocketAddr> + Unpin + Send + Sync + 'static>,
    ) -> anyhow::Result<()> {
        let mut g = self.locked.write();

        let peer_adder = |live: Weak<TorrentStateLive>| async move {
            {
                let live: Arc<TorrentStateLive> = live.upgrade().context("no longer live")?;
                for peer in initial_peers {
                    live.add_peer_if_not_seen(peer).context("torrent closed")?;
                }
            }

            if let Some(mut peer_rx) = peer_rx {
                while let Some(peer) = peer_rx.next().await {
                    live.upgrade()
                        .context("no longer live")?
                        .add_peer_if_not_seen(peer)
                        .context("torrent closed")?;
                }
            } else {
                error!("peer rx is not set");
            }

            Ok(())
        };

        let span = self.info.span.clone();

        match &g.state {
            ManagedTorrentState::Live(_) => {
                bail!("torrent is already live");
            }
            ManagedTorrentState::Initializing(init) => {
                let init = init.clone();
                let t = self.clone();
                spawn(
                    "initialize_and_start",
                    error_span!(parent: span.clone(), "initialize_and_start"),
                    async move {
                        match init.check().await {
                            Ok(paused) => {
                                let mut g = t.locked.write();
                                if let ManagedTorrentState::Initializing(_) = &g.state {
                                } else {
                                    debug!("no need to start torrent anymore, as it switched state from initilizing");
                                    return Ok(());
                                }

                                let live = TorrentStateLive::new(paused);
                                g.state = ManagedTorrentState::Live(live.clone());

                                spawn(
                                    "external_peer_adder",
                                    error_span!(parent: span.clone(), "external_peer_adder"),
                                    peer_adder(Arc::downgrade(&live)),
                                );

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
                let live = TorrentStateLive::new(paused);
                g.state = ManagedTorrentState::Live(live.clone());
                spawn(
                    "external_peer_adder",
                    error_span!(parent: span.clone(), "external_peer_adder"),
                    peer_adder(Arc::downgrade(&live)),
                );
                Ok(())
            }
            ManagedTorrentState::Error(_) => {
                bail!("starting torrents from error state not implemented")
            }
            ManagedTorrentState::None => bail!("bug: torrent is in empty state"),
        }
    }

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

    pub async fn wait_until_completed(&self) -> anyhow::Result<()> {
        // TODO: rewrite
        self.live()
            .context("torrent isn't live")?
            .wait_until_completed()
            .await;
        Ok(())
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
    trackers: Vec<Url>,
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

    pub fn trackers(&mut self, trackers: Vec<Url>) -> &mut Self {
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

    pub fn spawner(&mut self, spawner: BlockingSpawner) -> &mut Self {
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
