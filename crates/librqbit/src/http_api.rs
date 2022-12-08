use anyhow::Context;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use buffers::ByteString;
use dht::{Dht, DhtStats};
use librqbit_core::id20::Id20;
use librqbit_core::torrent_metainfo::TorrentMetaV1Info;
use log::warn;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{response, routing, Router};

use crate::session::{AddTorrentOptions, AddTorrentResponse, ListOnlyResponse, Session};
use crate::torrent_manager::TorrentManagerHandle;
use crate::torrent_state::StatsSnapshot;

type Result<T> = std::result::Result<T, ApiError>;

// Private HTTP API internals.
pub struct ApiInternal {
    dht: Option<Dht>,
    startup_time: Instant,
    torrent_managers: RwLock<Vec<TorrentManagerHandle>>,
    session: Arc<Session>,
}

// Convenience error type.
#[derive(Debug)]
struct ApiError {
    status: Option<StatusCode>,
    kind: ApiErrorKind,
}

impl ApiError {
    const fn torrent_not_found(torrent_id: usize) -> Self {
        Self {
            status: Some(StatusCode::NOT_FOUND),
            kind: ApiErrorKind::TorrentNotFound(torrent_id),
        }
    }
    const fn dht_disabled() -> Self {
        Self {
            status: Some(StatusCode::NOT_FOUND),
            kind: ApiErrorKind::DhtDisabled,
        }
    }
    fn with_status(self, status: StatusCode) -> Self {
        Self {
            status: Some(status),
            kind: self.kind,
        }
    }
}

#[derive(Debug)]
enum ApiErrorKind {
    TorrentNotFound(usize),
    DhtDisabled,
    Other(anyhow::Error),
}

impl From<anyhow::Error> for ApiError {
    fn from(value: anyhow::Error) -> Self {
        Self {
            status: None,
            kind: ApiErrorKind::Other(value),
        }
    }
}

impl From<(StatusCode, anyhow::Error)> for ApiError {
    fn from((code, err): (StatusCode, anyhow::Error)) -> Self {
        ApiError::from(err).with_status(code)
    }
}

impl std::error::Error for ApiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            ApiErrorKind::Other(err) => err.source(),
            _ => None,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            ApiErrorKind::TorrentNotFound(idx) => write!(f, "torrent {idx} not found"),
            ApiErrorKind::Other(err) => write!(f, "{err:?}"),
            ApiErrorKind::DhtDisabled => write!(f, "DHT is disabled"),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> response::Response {
        let mut response = format!("{self}").into_response();
        *response.status_mut() = match self.status {
            Some(s) => s,
            None => StatusCode::INTERNAL_SERVER_ERROR,
        };
        response
    }
}

impl ApiInternal {
    fn new(session: Arc<Session>) -> Self {
        Self {
            dht: session.get_dht(),
            startup_time: Instant::now(),
            torrent_managers: RwLock::new(Vec::new()),
            session,
        }
    }

    fn add_torrent_handle(&self, handle: TorrentManagerHandle) -> usize {
        let mut g = self.torrent_managers.write();
        let idx = g.len();
        g.push(handle);
        idx
    }
}

#[derive(Serialize)]
struct Speed {
    mbps: f64,
    human_readable: String,
}

impl Speed {
    fn new(mbps: f64) -> Self {
        Self {
            mbps,
            human_readable: format!("{mbps:.2} MiB/s"),
        }
    }
}

impl From<f64> for Speed {
    fn from(mbps: f64) -> Self {
        Self::new(mbps)
    }
}

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

#[derive(Serialize, Deserialize)]
pub struct TorrentDetailsResponse {
    pub info_hash: String,
    pub files: Vec<TorrentDetailsResponseFile>,
}

#[derive(Serialize)]
struct StatsResponse {
    snapshot: StatsSnapshot,
    average_piece_download_time: Option<Duration>,
    download_speed: Speed,
    all_time_download_speed: Speed,
    time_remaining: Option<Duration>,
}

#[derive(Serialize, Deserialize)]
pub struct ApiAddTorrentResponse {
    pub id: Option<usize>,
    pub details: TorrentDetailsResponse,
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

impl ApiInternal {
    fn mgr_handle(&self, idx: usize) -> Result<TorrentManagerHandle> {
        self.torrent_managers
            .read()
            .get(idx)
            .cloned()
            .ok_or(ApiError::torrent_not_found(idx))
    }

    fn api_torrent_list(&self) -> TorrentListResponse {
        TorrentListResponse {
            torrents: self
                .torrent_managers
                .read()
                .iter()
                .enumerate()
                .map(|(id, mgr)| TorrentListResponseItem {
                    id,
                    info_hash: mgr.torrent_state().info_hash().as_string(),
                })
                .collect(),
        }
    }

    fn api_torrent_details(&self, idx: usize) -> Result<TorrentDetailsResponse> {
        let handle = self.mgr_handle(idx)?;
        let info_hash = handle.torrent_state().info_hash();
        let only_files = handle.only_files();
        make_torrent_details(&info_hash, handle.torrent_state().info(), only_files)
    }

    async fn api_add_torrent(
        &self,
        url: String,
        opts: Option<AddTorrentOptions>,
    ) -> Result<ApiAddTorrentResponse> {
        let response = match self
            .session
            .add_torrent(&url, opts)
            .await
            .context("error adding torrent")?
        {
            AddTorrentResponse::AlreadyManaged(managed) => {
                return Err(ApiError::from((
                    StatusCode::CONFLICT,
                    anyhow::anyhow!(
                        "{:?} is already managed, downloaded to {:?}",
                        managed.info_hash,
                        managed.output_folder
                    ),
                )));
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
            AddTorrentResponse::Added(handle) => {
                let details = make_torrent_details(
                    &handle.torrent_state().info_hash(),
                    handle.torrent_state().info(),
                    handle.only_files(),
                )
                .context("error making torrent details")?;
                let id = self.add_torrent_handle(handle);
                ApiAddTorrentResponse {
                    id: Some(id),
                    details,
                }
            }
        };
        Ok(response)
    }

    fn api_dht_stats(&self) -> Option<DhtStats> {
        self.dht.as_ref().map(|d| d.stats())
    }

    fn api_stats(&self, idx: usize) -> Result<StatsResponse> {
        let mgr = self.mgr_handle(idx)?;
        let snapshot = mgr.torrent_state().stats_snapshot();
        let estimator = mgr.speed_estimator();

        // Poor mans download speed computation
        let elapsed = self.startup_time.elapsed();
        let downloaded_bytes = snapshot.downloaded_and_checked_bytes;
        let downloaded_mb = downloaded_bytes as f64 / 1024f64 / 1024f64;

        Ok(StatsResponse {
            average_piece_download_time: snapshot.average_piece_download_time(),
            snapshot,
            all_time_download_speed: (downloaded_mb / elapsed.as_secs_f64()).into(),
            download_speed: estimator.download_mbps().into(),
            time_remaining: estimator.time_remaining(),
        })
    }

    fn api_dump_haves(&self, idx: usize) -> Result<String> {
        let mgr = self.mgr_handle(idx)?;
        Ok(format!(
            "{:?}",
            mgr.torrent_state().lock_read().chunks.get_have_pieces(),
        ))
    }
}

type ApiState = Arc<ApiInternal>;

#[derive(Clone)]
pub struct HttpApi {
    inner: Arc<ApiInternal>,
}

#[derive(Serialize, Deserialize)]
pub struct TorrentAddQueryParams {
    pub overwrite: Option<bool>,
    pub output_folder: Option<String>,
    pub sub_folder: Option<String>,
    pub only_files_regex: Option<String>,
    pub list_only: Option<bool>,
}

async fn axum_post_torrent(
    State(state): State<ApiState>,
    Query(params): Query<TorrentAddQueryParams>,
    url: String,
) -> Result<axum::Json<ApiAddTorrentResponse>> {
    let opts = AddTorrentOptions {
        overwrite: params.overwrite.unwrap_or(false),
        only_files_regex: params.only_files_regex,
        output_folder: params.output_folder,
        sub_folder: params.sub_folder,
        list_only: params.list_only.unwrap_or(false),
        ..Default::default()
    };
    state
        .api_add_torrent(url, Some(opts))
        .await
        .map(axum::Json)
        .map_err(|e| e.with_status(StatusCode::BAD_REQUEST))
}

// Public API
impl HttpApi {
    pub fn new(session: Arc<Session>) -> Self {
        Self {
            inner: Arc::new(ApiInternal::new(session)),
        }
    }
    pub fn add_torrent_handle(&self, handle: TorrentManagerHandle) -> usize {
        self.inner.add_torrent_handle(handle)
    }

    pub async fn make_http_api_and_run(self, addr: SocketAddr) -> anyhow::Result<()> {
        let state = self.inner;
        let api_description_body = serde_json::json!({
            "apis": {
                "GET /": "list all available APIs",
                "GET /dht/stats": "DHT stats",
                "GET /dht/table": "DHT routing table",
                "GET /torrents": "List torrents (default torrent is 0)",
                "GET /torrents/{index}": "Torrent details",
                "GET /torrents/{index}/haves": "The bitfield of have pieces",
                "GET /torrents/{index}/stats": "Torrent stats",
                // This is kind of not secure as it just reads any local file that it has access to,
                // or any URL, but whatever, ok for our purposes / threat model.
                "POST /torrents": "Add a torrent here. magnet: or http:// or a local file."
            },
            "server": "rqbit",
        });

        let app = Router::new()
            .route(
                "/",
                routing::get(move || async move { axum::Json(api_description_body) }),
            )
            .route(
                "/dht/stats",
                routing::get(|State(state): State<ApiState>| async move {
                    let dht_stats = state.api_dht_stats().ok_or(ApiError::dht_disabled())?;
                    Ok::<_, ApiError>(axum::Json(dht_stats))
                }),
            )
            .route(
                "/dht/table",
                routing::get(|State(state): State<ApiState>| async move {
                    let dht = state.dht.as_ref().ok_or(ApiError::dht_disabled())?;
                    Ok::<_, ApiError>(dht.with_routing_table(|r| axum::Json(r.clone())))
                }),
            )
            .route(
                "/torrents",
                routing::get(move |State(state): State<ApiState>| async move {
                    axum::Json(state.api_torrent_list())
                }),
            )
            .route("/torrents", routing::post(axum_post_torrent))
            .route(
                "/torrents/:id",
                routing::get(
                    |State(state): State<ApiState>, Path(idx): Path<usize>| async move {
                        state.api_torrent_details(idx).map(axum::Json)
                    },
                ),
            )
            .route(
                "/torrents/:id/haves",
                routing::get(
                    |State(state): State<ApiState>, Path(idx): Path<usize>| async move {
                        state.api_dump_haves(idx)
                    },
                ),
            )
            .route(
                "/torrents/:id/stats",
                routing::get(
                    |State(state): State<ApiState>, Path(idx): Path<usize>| async move {
                        state.api_stats(idx).map(axum::Json)
                    },
                ),
            )
            .with_state(state);

        log::info!("starting HTTP server on {}", addr);
        axum::Server::try_bind(&addr)
            .with_context(|| format!("error binding to {addr}"))?
            .serve(app.into_make_service())
            .await?;
        Ok(())
    }
}
