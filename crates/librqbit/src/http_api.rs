use anyhow::Context;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use buffers::ByteString;
use dht::DhtStats;
use http::StatusCode;
use itertools::Itertools;
use librqbit_core::id20::Id20;
use librqbit_core::torrent_metainfo::TorrentMetaV1Info;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

use axum::Router;

use crate::http_api_error::{ApiError, ApiErrorExt};
use crate::peer_connection::PeerConnectionOptions;
use crate::session::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ListOnlyResponse, Session, TorrentId,
};
use crate::torrent_state::peer::stats::snapshot::{PeerStatsFilter, PeerStatsSnapshot};
use crate::torrent_state::stats::{LiveStats, TorrentStats};
use crate::torrent_state::ManagedTorrentHandle;

// Public API
#[derive(Clone)]
pub struct HttpApi {
    inner: Arc<ApiInternal>,
}

impl HttpApi {
    pub fn new(session: Arc<Session>, rust_log_reload_tx: Option<UnboundedSender<String>>) -> Self {
        Self {
            inner: Arc::new(ApiInternal::new(session, rust_log_reload_tx)),
        }
    }

    pub async fn make_http_api_and_run(
        self,
        addr: SocketAddr,
        read_only: bool,
    ) -> anyhow::Result<()> {
        let state = self.inner;

        async fn api_root() -> impl IntoResponse {
            axum::Json(serde_json::json!({
                "apis": {
                    "GET /": "list all available APIs",
                    "GET /dht/stats": "DHT stats",
                    "GET /dht/table": "DHT routing table",
                    "GET /torrents": "List torrents (default torrent is 0)",
                    "GET /torrents/{index}": "Torrent details",
                    "GET /torrents/{index}/haves": "The bitfield of have pieces",
                    "GET /torrents/{index}/stats/v1": "Torrent stats",
                    "GET /torrents/{index}/peer_stats": "Per peer stats",
                    "POST /torrents/{index}/pause": "Pause torrent",
                    "POST /torrents/{index}/start": "Resume torrent",
                    "POST /torrents/{index}/forget": "Forget about the torrent, keep the files",
                    "POST /torrents/{index}/delete": "Forget about the torrent, remove the files",
                    "POST /torrents": "Add a torrent here. magnet: or http:// or a local file.",
                    "POST /rust_log": "Set RUST_LOG to this post launch (for debugging)",
                    "GET /web/": "Web UI",
                },
                "server": "rqbit",
            }))
        }

        async fn dht_stats(State(state): State<ApiState>) -> Result<impl IntoResponse> {
            state.api_dht_stats().map(axum::Json)
        }

        async fn dht_table(State(state): State<ApiState>) -> Result<impl IntoResponse> {
            state.api_dht_table().map(axum::Json)
        }

        async fn torrents_list(State(state): State<ApiState>) -> impl IntoResponse {
            axum::Json(state.api_torrent_list())
        }

        async fn torrents_post(
            State(state): State<ApiState>,
            Query(params): Query<TorrentAddQueryParams>,
            data: Bytes,
        ) -> Result<impl IntoResponse> {
            let opts = params.into_add_torrent_options();
            let add = match String::from_utf8(data.to_vec()) {
                Ok(s) => AddTorrent::Url(s.into()),
                Err(e) => AddTorrent::TorrentFileBytes(e.into_bytes().into()),
            };
            state.api_add_torrent(add, Some(opts)).await.map(axum::Json)
        }

        async fn torrent_details(
            State(state): State<ApiState>,
            Path(idx): Path<usize>,
        ) -> Result<impl IntoResponse> {
            state.api_torrent_details(idx).map(axum::Json)
        }

        async fn torrent_haves(
            State(state): State<ApiState>,
            Path(idx): Path<usize>,
        ) -> Result<impl IntoResponse> {
            state.api_dump_haves(idx)
        }

        async fn torrent_stats_v0(
            State(state): State<ApiState>,
            Path(idx): Path<usize>,
        ) -> Result<impl IntoResponse> {
            state.api_stats_v0(idx).map(axum::Json)
        }

        async fn torrent_stats_v1(
            State(state): State<ApiState>,
            Path(idx): Path<usize>,
        ) -> Result<impl IntoResponse> {
            state.api_stats_v1(idx).map(axum::Json)
        }

        async fn peer_stats(
            State(state): State<ApiState>,
            Path(idx): Path<usize>,
            Query(filter): Query<PeerStatsFilter>,
        ) -> Result<impl IntoResponse> {
            state.api_peer_stats(idx, filter).map(axum::Json)
        }

        async fn torrent_action_pause(
            State(state): State<ApiState>,
            Path(idx): Path<usize>,
        ) -> Result<impl IntoResponse> {
            state.api_torrent_action_pause(idx).map(axum::Json)
        }

        async fn torrent_action_start(
            State(state): State<ApiState>,
            Path(idx): Path<usize>,
        ) -> Result<impl IntoResponse> {
            state.api_torrent_action_start(idx).map(axum::Json)
        }

        async fn torrent_action_forget(
            State(state): State<ApiState>,
            Path(idx): Path<usize>,
        ) -> Result<impl IntoResponse> {
            state.api_torrent_action_forget(idx).map(axum::Json)
        }

        async fn torrent_action_delete(
            State(state): State<ApiState>,
            Path(idx): Path<usize>,
        ) -> Result<impl IntoResponse> {
            state.api_torrent_action_delete(idx).map(axum::Json)
        }

        async fn set_rust_log(
            State(state): State<ApiState>,
            new_value: String,
        ) -> Result<impl IntoResponse> {
            state.api_set_rust_log(new_value).map(axum::Json)
        }

        let mut app = Router::new()
            .route("/", get(api_root))
            .route("/rust_log", post(set_rust_log))
            .route("/dht/stats", get(dht_stats))
            .route("/dht/table", get(dht_table))
            .route("/torrents", get(torrents_list))
            .route("/torrents/:id", get(torrent_details))
            .route("/torrents/:id/haves", get(torrent_haves))
            .route("/torrents/:id/stats", get(torrent_stats_v0))
            .route("/torrents/:id/stats/v1", get(torrent_stats_v1))
            .route("/torrents/:id/peer_stats", get(peer_stats));

        if !read_only {
            app = app
                .route("/torrents", post(torrents_post))
                .route("/torrents/:id/pause", post(torrent_action_pause))
                .route("/torrents/:id/start", post(torrent_action_start))
                .route("/torrents/:id/forget", post(torrent_action_forget))
                .route("/torrents/:id/delete", post(torrent_action_delete));
        }

        #[cfg(feature = "webui")]
        {
            let webui_router = Router::new()
                .route(
                    "/",
                    get(|| async {
                        (
                            [("Content-Type", "text/html")],
                            include_str!("../webui/dist/index.html"),
                        )
                    }),
                )
                .route(
                    "/assets/index.js",
                    get(|| async {
                        (
                            [("Content-Type", "application/javascript")],
                            include_str!("../webui/dist/assets/index.js"),
                        )
                    }),
                )
                .route(
                    "/assets/logo.svg",
                    get(|| async {
                        (
                            [("Content-Type", "image/svg+xml")],
                            include_str!("../webui/dist/assets/logo.svg"),
                        )
                    }),
                );

            // This is to develop webui by just doing "open index.html && tsc --watch"
            let cors_layer = std::env::var("CORS_DEBUG")
                .ok()
                .map(|_| {
                    use tower_http::cors::{AllowHeaders, AllowOrigin};

                    warn!("CorsLayer: allowing everything because CORS_DEBUG is set");
                    tower_http::cors::CorsLayer::default()
                        .allow_origin(AllowOrigin::predicate(|_, _| true))
                        .allow_headers(AllowHeaders::any())
                })
                .unwrap_or_default();

            app = app.nest("/web/", webui_router).layer(cors_layer);
        }

        let app = app
            .layer(tower_http::trace::TraceLayer::new_for_http())
            .with_state(state)
            .into_make_service();

        info!("starting HTTP server on {}", addr);
        axum::Server::try_bind(&addr)
            .with_context(|| format!("error binding to {addr}"))?
            .serve(app)
            .await?;
        Ok(())
    }
}

type Result<T> = std::result::Result<T, ApiError>;

#[derive(Serialize)]
struct TorrentListResponseItem {
    id: usize,
    info_hash: String,
}

#[derive(Serialize)]
struct TorrentListResponse {
    torrents: Vec<TorrentListResponseItem>,
}

#[derive(Serialize, Deserialize)]
pub struct TorrentDetailsResponseFile {
    pub name: String,
    pub length: u64,
    pub included: bool,
}

#[derive(Default, Serialize)]
struct EmptyJsonResponse {}

#[derive(Serialize, Deserialize)]
pub struct TorrentDetailsResponse {
    pub info_hash: String,
    pub files: Vec<TorrentDetailsResponseFile>,
}

#[derive(Serialize, Deserialize)]
pub struct ApiAddTorrentResponse {
    pub id: Option<usize>,
    pub details: TorrentDetailsResponse,
}

pub struct OnlyFiles(Vec<usize>);

impl Serialize for OnlyFiles {
    fn serialize<S>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let s = self.0.iter().map(|id| id.to_string()).join(",");
        s.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for OnlyFiles {
    fn deserialize<D>(deserializer: D) -> core::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;

        let s = String::deserialize(deserializer)?;
        let list = s
            .split(',')
            .try_fold(Vec::<usize>::new(), |mut acc, c| match c.parse() {
                Ok(i) => {
                    acc.push(i);
                    Ok(acc)
                }
                Err(_) => Err(D::Error::custom(format!(
                    "only_files: failed to parse {:?} as integer",
                    c
                ))),
            })?;
        if list.is_empty() {
            return Err(D::Error::custom(
                "only_files: should contain at least one file id",
            ));
        }
        Ok(OnlyFiles(list))
    }
}

#[derive(Serialize, Deserialize, Default)]
pub struct TorrentAddQueryParams {
    pub overwrite: Option<bool>,
    pub output_folder: Option<String>,
    pub sub_folder: Option<String>,
    pub only_files_regex: Option<String>,
    pub only_files: Option<OnlyFiles>,
    pub peer_connect_timeout: Option<u64>,
    pub peer_read_write_timeout: Option<u64>,
    pub list_only: Option<bool>,
}

impl TorrentAddQueryParams {
    fn into_add_torrent_options(self) -> AddTorrentOptions {
        AddTorrentOptions {
            overwrite: self.overwrite.unwrap_or(false),
            only_files_regex: self.only_files_regex,
            only_files: self.only_files.map(|o| o.0),
            output_folder: self.output_folder,
            sub_folder: self.sub_folder,
            list_only: self.list_only.unwrap_or(false),
            peer_opts: Some(PeerConnectionOptions {
                connect_timeout: self.peer_connect_timeout.map(Duration::from_secs),
                read_write_timeout: self.peer_read_write_timeout.map(Duration::from_secs),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

// Private HTTP API internals. Agnostic of web framework.
struct ApiInternal {
    session: Arc<Session>,
    rust_log_reload_tx: Option<UnboundedSender<String>>,
}

type ApiState = Arc<ApiInternal>;

impl ApiInternal {
    pub fn new(session: Arc<Session>, rust_log_reload_tx: Option<UnboundedSender<String>>) -> Self {
        Self {
            session,
            rust_log_reload_tx,
        }
    }

    fn mgr_handle(&self, idx: TorrentId) -> Result<ManagedTorrentHandle> {
        self.session
            .get(idx)
            .ok_or(ApiError::torrent_not_found(idx))
    }

    fn api_torrent_list(&self) -> TorrentListResponse {
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

    fn api_torrent_details(&self, idx: TorrentId) -> Result<TorrentDetailsResponse> {
        let handle = self.mgr_handle(idx)?;
        let info_hash = handle.info().info_hash;
        let only_files = handle.only_files();
        make_torrent_details(&info_hash, &handle.info().info, only_files.as_deref())
    }

    fn api_peer_stats(&self, idx: TorrentId, filter: PeerStatsFilter) -> Result<PeerStatsSnapshot> {
        let handle = self.mgr_handle(idx)?;
        Ok(handle
            .live()
            .context("not live")?
            .per_peer_stats_snapshot(filter))
    }

    fn api_torrent_action_pause(&self, idx: TorrentId) -> Result<EmptyJsonResponse> {
        let handle = self.mgr_handle(idx)?;
        handle
            .pause()
            .context("error pausing torrent")
            .with_error_status_code(StatusCode::BAD_REQUEST)?;
        Ok(Default::default())
    }

    fn api_torrent_action_start(&self, idx: TorrentId) -> Result<EmptyJsonResponse> {
        let handle = self.mgr_handle(idx)?;
        self.session
            .unpause(&handle)
            .context("error unpausing torrent")
            .with_error_status_code(StatusCode::BAD_REQUEST)?;
        Ok(Default::default())
    }

    fn api_torrent_action_forget(&self, idx: TorrentId) -> Result<EmptyJsonResponse> {
        self.session
            .delete(idx, false)
            .context("error forgetting torrent")?;
        Ok(Default::default())
    }

    fn api_torrent_action_delete(&self, idx: TorrentId) -> Result<EmptyJsonResponse> {
        self.session
            .delete(idx, true)
            .context("error deleting torrent with files")?;
        Ok(Default::default())
    }

    fn api_set_rust_log(&self, new_value: String) -> Result<EmptyJsonResponse> {
        let tx = self
            .rust_log_reload_tx
            .as_ref()
            .context("rust_log_reload_tx was not set")?;
        tx.send(new_value)
            .context("noone is listening to RUST_LOG changes")?;
        Ok(Default::default())
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
            }) => ApiAddTorrentResponse {
                id: None,
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
                }
            }
        };
        Ok(response)
    }

    fn api_dht_stats(&self) -> Result<DhtStats> {
        self.session
            .get_dht()
            .as_ref()
            .map(|d| d.stats())
            .ok_or(ApiError::dht_disabled())
    }

    fn api_dht_table(&self) -> Result<impl Serialize> {
        let dht = self.session.get_dht().ok_or(ApiError::dht_disabled())?;
        Ok(dht.with_routing_table(|r| r.clone()))
    }

    fn api_stats_v0(&self, idx: TorrentId) -> Result<LiveStats> {
        let mgr = self.mgr_handle(idx)?;
        let live = mgr.live().context("torrent not live")?;
        Ok(LiveStats::from(&*live))
    }

    fn api_stats_v1(&self, idx: TorrentId) -> Result<TorrentStats> {
        let mgr = self.mgr_handle(idx)?;
        Ok(mgr.stats())
    }

    fn api_dump_haves(&self, idx: usize) -> Result<String> {
        let mgr = self.mgr_handle(idx)?;
        Ok(mgr.with_chunk_tracker(|chunks| format!("{:?}", chunks.get_have_pieces()))?)
    }
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
            let included = only_files.map(|o| o.contains(&idx)).unwrap_or(true);
            TorrentDetailsResponseFile {
                name,
                length,
                included,
            }
        })
        .collect();
    Ok(TorrentDetailsResponse {
        info_hash: info_hash.as_string(),
        files,
    })
}
