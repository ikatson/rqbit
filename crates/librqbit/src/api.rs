use std::{net::SocketAddr, sync::Arc};

use anyhow::Context;
use buffers::ByteString;
use dht::{DhtStats, Id20};
use futures::Stream;
use http::StatusCode;
use librqbit_core::torrent_metainfo::TorrentMetaV1Info;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use tracing::warn;

use crate::{
    api_error::{ApiError, ApiErrorExt},
    session::{
        AddTorrent, AddTorrentOptions, AddTorrentResponse, ListOnlyResponse, Session, TorrentId,
    },
    torrent_state::{
        peer::stats::snapshot::{PeerStatsFilter, PeerStatsSnapshot},
        ManagedTorrentHandle,
    },
    tracing_subscriber_config_utils::LineBroadcast,
};

pub use crate::torrent_state::stats::{LiveStats, TorrentStats};

pub type Result<T> = std::result::Result<T, ApiError>;

/// Library API for use in different web frameworks.
/// Contains all methods you might want to expose with (de)serializable inputs/outputs.
#[derive(Clone)]
pub struct Api {
    session: Arc<Session>,
    rust_log_reload_tx: Option<UnboundedSender<String>>,
    line_broadcast: Option<LineBroadcast>,
}

impl Api {
    pub fn new(
        session: Arc<Session>,
        rust_log_reload_tx: Option<UnboundedSender<String>>,
        line_broadcast: Option<LineBroadcast>,
    ) -> Self {
        Self {
            session,
            rust_log_reload_tx,
            line_broadcast,
        }
    }

    pub fn session(&self) -> &Arc<Session> {
        &self.session
    }

    pub fn mgr_handle(&self, idx: TorrentId) -> Result<ManagedTorrentHandle> {
        self.session
            .get(idx)
            .ok_or(ApiError::torrent_not_found(idx))
    }

    pub fn api_torrent_list(&self) -> TorrentListResponse {
        let items = self.session.with_torrents(|torrents| {
            torrents
                .map(|(id, mgr)| TorrentListResponseItem {
                    id,
                    info_hash: mgr.info().info_hash.as_string(),
                })
                .collect()
        });
        TorrentListResponse { torrents: items }
    }

    pub fn api_torrent_details(&self, idx: TorrentId) -> Result<TorrentDetailsResponse> {
        let handle = self.mgr_handle(idx)?;
        let info_hash = handle.info().info_hash;
        let only_files = handle.only_files();
        make_torrent_details(&info_hash, &handle.info().info, only_files.as_deref())
    }

    pub fn api_peer_stats(
        &self,
        idx: TorrentId,
        filter: PeerStatsFilter,
    ) -> Result<PeerStatsSnapshot> {
        let handle = self.mgr_handle(idx)?;
        Ok(handle
            .live()
            .context("not live")?
            .per_peer_stats_snapshot(filter))
    }

    pub fn api_torrent_action_pause(&self, idx: TorrentId) -> Result<EmptyJsonResponse> {
        let handle = self.mgr_handle(idx)?;
        handle
            .pause()
            .context("error pausing torrent")
            .with_error_status_code(StatusCode::BAD_REQUEST)?;
        Ok(Default::default())
    }

    pub fn api_torrent_action_start(&self, idx: TorrentId) -> Result<EmptyJsonResponse> {
        let handle = self.mgr_handle(idx)?;
        self.session
            .unpause(&handle)
            .context("error unpausing torrent")
            .with_error_status_code(StatusCode::BAD_REQUEST)?;
        Ok(Default::default())
    }

    pub fn api_torrent_action_forget(&self, idx: TorrentId) -> Result<EmptyJsonResponse> {
        self.session
            .delete(idx, false)
            .context("error forgetting torrent")?;
        Ok(Default::default())
    }

    pub fn api_torrent_action_delete(&self, idx: TorrentId) -> Result<EmptyJsonResponse> {
        self.session
            .delete(idx, true)
            .context("error deleting torrent with files")?;
        Ok(Default::default())
    }

    pub fn api_set_rust_log(&self, new_value: String) -> Result<EmptyJsonResponse> {
        let tx = self
            .rust_log_reload_tx
            .as_ref()
            .context("rust_log_reload_tx was not set")?;
        tx.send(new_value)
            .context("noone is listening to RUST_LOG changes")?;
        Ok(Default::default())
    }

    pub fn api_log_lines_stream(
        &self,
    ) -> Result<
        impl Stream<Item = std::result::Result<bytes::Bytes, BroadcastStreamRecvError>>
            + Send
            + Sync
            + 'static,
    > {
        Ok(self
            .line_broadcast
            .as_ref()
            .map(|sender| BroadcastStream::new(sender.subscribe()))
            .context("line_rx wasn't set")?)
    }

    pub async fn api_add_torrent(
        &self,
        add: AddTorrent<'_>,
        opts: Option<AddTorrentOptions>,
    ) -> Result<ApiAddTorrentResponse> {
        let response = match self
            .session
            .add_torrent(add, opts)
            .await
            .context("error adding torrent")
            .with_error_status_code(StatusCode::BAD_REQUEST)?
        {
            AddTorrentResponse::AlreadyManaged(id, managed) => {
                return Err(anyhow::anyhow!(
                    "{:?} is already managed, id={}, downloaded to {:?}",
                    managed.info_hash(),
                    id,
                    &managed.info().out_dir
                ))
                .with_error_status_code(StatusCode::CONFLICT);
            }
            AddTorrentResponse::ListOnly(ListOnlyResponse {
                info_hash,
                info,
                only_files,
                seen_peers,
                output_folder,
            }) => ApiAddTorrentResponse {
                id: None,
                output_folder: output_folder.to_string_lossy().into_owned(),
                seen_peers: Some(seen_peers),
                details: make_torrent_details(&info_hash, &info, only_files.as_deref())
                    .context("error making torrent details")?,
            },
            AddTorrentResponse::Added(id, handle) => {
                let details = make_torrent_details(
                    &handle.info_hash(),
                    &handle.info().info,
                    handle.only_files().as_deref(),
                )
                .context("error making torrent details")?;
                ApiAddTorrentResponse {
                    id: Some(id),
                    details,
                    output_folder: handle.info().out_dir.to_string_lossy().into_owned(),
                    seen_peers: None,
                }
            }
        };
        Ok(response)
    }

    pub fn api_dht_stats(&self) -> Result<DhtStats> {
        self.session
            .get_dht()
            .as_ref()
            .map(|d| d.stats())
            .ok_or(ApiError::dht_disabled())
    }

    pub fn api_dht_table(&self) -> Result<impl Serialize> {
        let dht = self.session.get_dht().ok_or(ApiError::dht_disabled())?;
        Ok(dht.with_routing_table(|r| r.clone()))
    }

    pub fn api_stats_v0(&self, idx: TorrentId) -> Result<LiveStats> {
        let mgr = self.mgr_handle(idx)?;
        let live = mgr.live().context("torrent not live")?;
        Ok(LiveStats::from(&*live))
    }

    pub fn api_stats_v1(&self, idx: TorrentId) -> Result<TorrentStats> {
        let mgr = self.mgr_handle(idx)?;
        Ok(mgr.stats())
    }

    pub fn api_dump_haves(&self, idx: usize) -> Result<String> {
        let mgr = self.mgr_handle(idx)?;
        Ok(mgr.with_chunk_tracker(|chunks| format!("{:?}", chunks.get_have_pieces()))?)
    }
}

#[derive(Serialize)]
pub struct TorrentListResponseItem {
    pub id: usize,
    pub info_hash: String,
}

#[derive(Serialize)]
pub struct TorrentListResponse {
    pub torrents: Vec<TorrentListResponseItem>,
}

#[derive(Serialize, Deserialize)]
pub struct TorrentDetailsResponseFile {
    pub name: String,
    pub components: Vec<String>,
    pub length: u64,
    pub included: bool,
}

#[derive(Default, Serialize)]
pub struct EmptyJsonResponse {}

#[derive(Serialize, Deserialize)]
pub struct TorrentDetailsResponse {
    pub info_hash: String,
    pub name: Option<String>,
    pub files: Vec<TorrentDetailsResponseFile>,
}

#[derive(Serialize, Deserialize)]
pub struct ApiAddTorrentResponse {
    pub id: Option<usize>,
    pub details: TorrentDetailsResponse,
    pub output_folder: String,
    pub seen_peers: Option<Vec<SocketAddr>>,
}

fn make_torrent_details(
    info_hash: &Id20,
    info: &TorrentMetaV1Info<ByteString>,
    only_files: Option<&[usize]>,
) -> Result<TorrentDetailsResponse> {
    let files = info
        .iter_filenames_and_lengths()
        .context("error iterating filenames and lengths")?
        .enumerate()
        .map(|(idx, (filename_it, length))| {
            let name = match filename_it.to_string() {
                Ok(s) => s,
                Err(err) => {
                    warn!("error reading filename: {:?}", err);
                    "<INVALID NAME>".to_string()
                }
            };
            let components = filename_it.to_vec().unwrap_or_default();
            let included = only_files.map(|o| o.contains(&idx)).unwrap_or(true);
            TorrentDetailsResponseFile {
                name,
                components,
                length,
                included,
            }
        })
        .collect();
    Ok(TorrentDetailsResponse {
        info_hash: info_hash.as_string(),
        name: info.name.as_ref().map(|b| b.to_string()),
        files,
    })
}
