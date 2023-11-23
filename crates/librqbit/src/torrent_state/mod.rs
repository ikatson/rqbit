pub mod utils;

pub mod live;

use std::sync::Arc;

use buffers::ByteString;
use librqbit_core::id20::Id20;
use librqbit_core::torrent_metainfo::TorrentMetaV1Info;
pub use live::*;
use parking_lot::RwLock;
use tokio::sync::mpsc::Sender;
use url::Url;

pub(crate) enum ManagedTorrentState {
    Live {
        state: TorrentStateLive,
        only_files_tx: Sender<Vec<usize>>,
        trackers_tx: Sender<Url>,
    },
}

pub(crate) struct ManagedTorrentLocked {
    pub trackers: Vec<Url>,
    pub only_files: Vec<usize>,
    pub state: ManagedTorrentState,
}

pub struct ManagedTorrentInfo {
    pub info: TorrentMetaV1Info<ByteString>,
    pub info_hash: Id20,
}

pub(crate) struct ManagedTorrent {
    pub info: Arc<ManagedTorrentInfo>,
    pub(crate) locked: RwLock<ManagedTorrentLocked>,
}
