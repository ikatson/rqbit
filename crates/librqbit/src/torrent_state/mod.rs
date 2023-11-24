pub mod initializing;
pub mod live;
pub mod paused;
pub mod utils;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::{collections::HashSet, path::Path};

use anyhow::Context;
use buffers::ByteString;
use librqbit_core::id20::Id20;
use librqbit_core::peer_id::generate_peer_id;
use librqbit_core::speed_estimator::SpeedEstimator;
use librqbit_core::torrent_metainfo::TorrentMetaV1Info;
pub use live::*;
use parking_lot::RwLock;
use tokio::sync::mpsc::Sender;
use tracing::trace_span;
use url::Url;

use crate::spawn_utils::{spawn, BlockingSpawner};

use initializing::TorrentStateInitializing;

use self::paused::TorrentStatePaused;

pub enum ManagedTorrentState {
    Initializing(TorrentStateInitializing),
    Paused(TorrentStatePaused),
    Live(Arc<TorrentStateLive>),
    Error(anyhow::Error),
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
    pub trackers: Vec<Url>,
    pub peer_id: Id20,
    pub(crate) options: ManagedTorrentOptions,
}

pub struct ManagedTorrent {
    pub info: Arc<ManagedTorrentInfo>,
    locked: RwLock<ManagedTorrentLocked>,
}

impl ManagedTorrent {
    pub fn info(&self) -> &ManagedTorrentInfo {
        &self.info
    }

    pub fn info_hash(&self) -> Id20 {
        self.info.info_hash
    }

    pub(crate) fn add_peer(&self, peer: SocketAddr) -> bool {
        todo!()
    }

    pub fn only_files(&self) -> Option<Vec<usize>> {
        // self.locked.write().only_files.clone()
        todo!()
    }

    pub fn with_state<R>(&self, f: impl FnOnce(&ManagedTorrentState) -> R) -> R {
        f(&self.locked.read().state)
    }

    pub fn live(&self) -> Option<Arc<TorrentStateLive>> {
        let g = self.locked.read();
        match &g.state {
            ManagedTorrentState::Live(live) => Some(live.clone()),
            _ => None,
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

    pub(crate) fn build(self) -> ManagedTorrentHandle {
        let info = Arc::new(ManagedTorrentInfo {
            info: self.info,
            info_hash: self.info_hash,
            out_dir: self.output_folder,
            trackers: self.trackers.into_iter().collect(),
            spawner: self.spawner.unwrap_or_default(),
            peer_id: self.peer_id.unwrap_or_else(generate_peer_id),
            options: ManagedTorrentOptions {
                force_tracker_interval: self.force_tracker_interval,
                peer_connect_timeout: self.peer_connect_timeout,
                peer_read_write_timeout: self.peer_read_write_timeout,
                overwrite: self.overwrite,
            },
        });
        let initializing = TorrentStateInitializing::new(info.clone(), self.only_files);
        Arc::new(ManagedTorrent {
            locked: RwLock::new(ManagedTorrentLocked {
                state: ManagedTorrentState::Initializing(initializing),
            }),
            info,
        })
    }
}

pub type ManagedTorrentHandle = Arc<ManagedTorrent>;
