use std::{collections::HashSet, marker::PhantomData, net::SocketAddr, str::FromStr, sync::Arc};

use anyhow::Context;
use buffers::ByteBufOwned;
use dht::{DhtStats, Id20};
use http::StatusCode;
use librqbit_core::torrent_metainfo::TorrentMetaV1Info;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use tracing::warn;

use crate::{
    api_error::{ApiError, ApiErrorExt},
    session::{
        AddTorrent, AddTorrentOptions, AddTorrentResponse, ListOnlyResponse, Session, TorrentId,
    },
    torrent_state::{
        peer::stats::snapshot::{PeerStatsFilter, PeerStatsSnapshot},
        FileStream, ManagedTorrentHandle,
    },
};

#[cfg(feature = "tracing-subscriber-utils")]
use crate::tracing_subscriber_config_utils::LineBroadcast;
#[cfg(feature = "tracing-subscriber-utils")]
use futures::Stream;
#[cfg(feature = "tracing-subscriber-utils")]
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};

pub use crate::torrent_state::stats::{LiveStats, TorrentStats};

pub type Result<T> = std::result::Result<T, ApiError>;

/// Library API for use in different web frameworks.
/// Contains all methods you might want to expose with (de)serializable inputs/outputs.
#[derive(Clone)]
pub struct Api {
    session: Arc<Session>,
    rust_log_reload_tx: Option<UnboundedSender<String>>,
    #[cfg(feature = "tracing-subscriber-utils")]
    line_broadcast: Option<LineBroadcast>,
}

#[derive(Debug, Clone, Copy)]
pub enum TorrentIdOrHash {
    Id(TorrentId),
    Hash(Id20),
}

impl Serialize for TorrentIdOrHash {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            TorrentIdOrHash::Id(id) => id.serialize(serializer),
            TorrentIdOrHash::Hash(h) => h.as_string().serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for TorrentIdOrHash {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Default)]
        struct V<'de> {
            p: PhantomData<&'de ()>,
        }
        impl<'de> serde::de::Visitor<'de> for V<'de> {
            type Value = TorrentIdOrHash;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("integer or 40 byte info hash")
            }

            fn visit_str<E>(self, v: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                TorrentIdOrHash::parse(v)
                    .map_err(|_| E::custom("expected integer or 40 byte info hash"))
            }
        }

        deserializer.deserialize_str(V::default())
    }
}

impl std::fmt::Display for TorrentIdOrHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TorrentIdOrHash::Id(id) => write!(f, "{}", id),
            TorrentIdOrHash::Hash(h) => write!(f, "{:?}", h),
        }
    }
}

impl From<TorrentId> for TorrentIdOrHash {
    fn from(value: TorrentId) -> Self {
        TorrentIdOrHash::Id(value)
    }
}

impl From<Id20> for TorrentIdOrHash {
    fn from(value: Id20) -> Self {
        TorrentIdOrHash::Hash(value)
    }
}

impl<'a> TryFrom<&'a str> for TorrentIdOrHash {
    type Error = anyhow::Error;

    fn try_from(value: &'a str) -> std::result::Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl TorrentIdOrHash {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        if s.len() == 40 {
            let id = Id20::from_str(s)?;
            return Ok(id.into());
        }
        let id: TorrentId = s.parse()?;
        Ok(id.into())
    }
}

impl Api {
    pub fn new(
        session: Arc<Session>,
        rust_log_reload_tx: Option<UnboundedSender<String>>,
        #[cfg(feature = "tracing-subscriber-utils")] line_broadcast: Option<LineBroadcast>,
    ) -> Self {
        Self {
            session,
            rust_log_reload_tx,
            #[cfg(feature = "tracing-subscriber-utils")]
            line_broadcast,
        }
    }

    pub fn session(&self) -> &Arc<Session> {
        &self.session
    }

    pub fn mgr_handle(&self, idx: TorrentIdOrHash) -> Result<ManagedTorrentHandle> {
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

    pub fn api_torrent_details(&self, idx: TorrentIdOrHash) -> Result<TorrentDetailsResponse> {
        let handle = self.mgr_handle(idx)?;
        let info_hash = handle.info().info_hash;
        let only_files = handle.only_files();
        make_torrent_details(&info_hash, &handle.info().info, only_files.as_deref())
    }

    pub fn torrent_file_mime_type(
        &self,
        idx: TorrentIdOrHash,
        file_idx: usize,
    ) -> Result<&'static str> {
        let handle = self.mgr_handle(idx)?;
        let info = &handle.info().info;
        torrent_file_mime_type(info, file_idx)
    }

    pub fn api_peer_stats(
        &self,
        idx: TorrentIdOrHash,
        filter: PeerStatsFilter,
    ) -> Result<PeerStatsSnapshot> {
        let handle = self.mgr_handle(idx)?;
        Ok(handle
            .live()
            .context("not live")?
            .per_peer_stats_snapshot(filter))
    }

    pub async fn api_torrent_action_pause(
        &self,
        idx: TorrentIdOrHash,
    ) -> Result<EmptyJsonResponse> {
        let handle = self.mgr_handle(idx)?;
        self.session()
            .pause(&handle)
            .await
            .context("error pausing torrent")
            .with_error_status_code(StatusCode::BAD_REQUEST)?;
        Ok(Default::default())
    }

    pub async fn api_torrent_action_start(
        &self,
        idx: TorrentIdOrHash,
    ) -> Result<EmptyJsonResponse> {
        let handle = self.mgr_handle(idx)?;
        self.session
            .unpause(&handle)
            .await
            .context("error unpausing torrent")
            .with_error_status_code(StatusCode::BAD_REQUEST)?;
        Ok(Default::default())
    }

    pub async fn api_torrent_action_forget(
        &self,
        idx: TorrentIdOrHash,
    ) -> Result<EmptyJsonResponse> {
        self.session
            .delete(idx, false)
            .await
            .context("error forgetting torrent")?;
        Ok(Default::default())
    }

    pub async fn api_torrent_action_delete(
        &self,
        idx: TorrentIdOrHash,
    ) -> Result<EmptyJsonResponse> {
        self.session
            .delete(idx, true)
            .await
            .context("error deleting torrent with files")?;
        Ok(Default::default())
    }

    pub async fn api_torrent_action_update_only_files(
        &self,
        idx: TorrentIdOrHash,
        only_files: &HashSet<usize>,
    ) -> Result<EmptyJsonResponse> {
        let handle = self.mgr_handle(idx)?;
        self.session
            .update_only_files(&handle, only_files)
            .await
            .context("error updating only_files")?;
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

    #[cfg(feature = "tracing-subscriber-utils")]
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
                    "{:?} is already managed, id={}",
                    managed.info_hash(),
                    id,
                ))
                .with_error_status_code(StatusCode::CONFLICT);
            }
            AddTorrentResponse::ListOnly(ListOnlyResponse {
                info_hash,
                info,
                only_files,
                seen_peers,
                output_folder,
                ..
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
                    seen_peers: None,
                    output_folder: handle
                        .info()
                        .options
                        .output_folder
                        .to_string_lossy()
                        .into_owned(),
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

    pub fn api_stats_v0(&self, idx: TorrentIdOrHash) -> Result<LiveStats> {
        let mgr = self.mgr_handle(idx)?;
        let live = mgr.live().context("torrent not live")?;
        Ok(LiveStats::from(&*live))
    }

    pub fn api_stats_v1(&self, idx: TorrentIdOrHash) -> Result<TorrentStats> {
        let mgr = self.mgr_handle(idx)?;
        Ok(mgr.stats())
    }

    pub fn api_dump_haves(&self, idx: TorrentIdOrHash) -> Result<String> {
        let mgr = self.mgr_handle(idx)?;
        Ok(mgr.with_chunk_tracker(|chunks| format!("{:?}", chunks.get_have_pieces()))?)
    }

    pub fn api_stream(&self, idx: TorrentIdOrHash, file_id: usize) -> Result<FileStream> {
        let mgr = self.mgr_handle(idx)?;
        Ok(mgr.stream(file_id)?)
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
    info: &TorrentMetaV1Info<ByteBufOwned>,
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

fn torrent_file_mime_type(
    info: &TorrentMetaV1Info<ByteBufOwned>,
    file_idx: usize,
) -> Result<&'static str> {
    info.iter_filenames_and_lengths()?
        .nth(file_idx)
        .and_then(|(f, _)| {
            f.iter_components()
                .last()
                .and_then(|r| r.ok())
                .and_then(|s| mime_guess::from_path(s).first_raw())
        })
        .ok_or_else(|| {
            ApiError::new_from_text(
                StatusCode::INTERNAL_SERVER_ERROR,
                "cannot determine mime type for file",
            )
        })
}
