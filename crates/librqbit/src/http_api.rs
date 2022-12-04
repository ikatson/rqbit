use anyhow::Context;
use axum::extract::{Query, State};
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

pub struct ApiInternal {
    dht: Option<Dht>,
    startup_time: Instant,
    torrent_managers: RwLock<Vec<TorrentManagerHandle>>,
    session: Arc<Session>,
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

    fn add_mgr(&self, handle: TorrentManagerHandle) -> usize {
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
) -> TorrentDetailsResponse {
    let files = info
        .iter_filenames_and_lengths()
        .unwrap()
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
    TorrentDetailsResponse {
        info_hash: info_hash.as_string(),
        files,
    }
}

impl ApiInternal {
    fn mgr_handle(&self, idx: usize) -> Option<TorrentManagerHandle> {
        self.torrent_managers.read().get(idx).cloned()
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

    fn api_torrent_details(&self, idx: usize) -> Option<TorrentDetailsResponse> {
        let handle = self.mgr_handle(idx)?;
        let info_hash = handle.torrent_state().info_hash();
        let only_files = handle.only_files();
        Some(make_torrent_details(
            &info_hash,
            handle.torrent_state().info(),
            only_files,
        ))
    }

    async fn api_add_torrent(
        &self,
        url: String,
        opts: Option<AddTorrentOptions>,
    ) -> anyhow::Result<ApiAddTorrentResponse> {
        let response = match self
            .session
            .add_torrent(&url, opts)
            .await
            .context("error adding torrent")?
        {
            AddTorrentResponse::AlreadyManaged(managed) => anyhow::bail!(
                "{:?} is already managed, downloaded to {:?}",
                managed.info_hash,
                managed.output_folder
            ),
            AddTorrentResponse::ListOnly(ListOnlyResponse {
                info_hash,
                info,
                only_files,
            }) => ApiAddTorrentResponse {
                id: None,
                details: make_torrent_details(&info_hash, &info, only_files.as_deref()),
            },
            AddTorrentResponse::Added(handle) => {
                let details = make_torrent_details(
                    &handle.torrent_state().info_hash(),
                    handle.torrent_state().info(),
                    handle.only_files(),
                );
                let id = self.add_mgr(handle);
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

    fn api_stats(&self, idx: usize) -> Option<StatsResponse> {
        let mgr = self.mgr_handle(idx)?;
        let snapshot = mgr.torrent_state().stats_snapshot();
        let estimator = mgr.speed_estimator();

        // Poor mans download speed computation
        let elapsed = self.startup_time.elapsed();
        let downloaded_bytes = snapshot.downloaded_and_checked_bytes;
        let downloaded_mb = downloaded_bytes as f64 / 1024f64 / 1024f64;

        Some(StatsResponse {
            average_piece_download_time: snapshot.average_piece_download_time(),
            snapshot,
            all_time_download_speed: (downloaded_mb / elapsed.as_secs_f64()).into(),
            download_speed: estimator.download_mbps().into(),
            time_remaining: estimator.time_remaining(),
        })
    }

    fn api_dump_haves(&self, idx: usize) -> Option<String> {
        let mgr = self.mgr_handle(idx)?;
        Some(format!(
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

fn axum_not_found_response<B: IntoResponse>(body: B) -> (StatusCode, B) {
    (StatusCode::NOT_FOUND, body)
}

fn axum_torrent_not_found_response(idx: usize) -> impl IntoResponse {
    axum_not_found_response(format!("torrent {idx} not found"))
}

fn axum_json_or_torrent_not_found<T: Serialize>(
    idx: usize,
    v: Option<T>,
) -> Result<axum::Json<T>, impl IntoResponse> {
    match v {
        Some(v) => Ok(axum::Json(v)),
        None => Err(axum_torrent_not_found_response(idx)),
    }
}

#[derive(Serialize, Deserialize)]
pub struct TorrentAddQueryParams {
    pub overwrite: Option<bool>,
    pub output_folder: Option<String>,
    pub sub_folder: Option<String>,
    pub only_files_regex: Option<String>,
    pub list_only: Option<bool>,
}

async fn post_torrent(
    State(inner): State<ApiState>,
    Query(params): Query<TorrentAddQueryParams>,
    url: String,
) -> Result<axum::Json<impl Serialize>, impl IntoResponse> {
    let opts = AddTorrentOptions {
        overwrite: params.overwrite.unwrap_or(false),
        only_files_regex: params.only_files_regex,
        output_folder: params.output_folder,
        sub_folder: params.sub_folder,
        list_only: params.list_only.unwrap_or(false),
        ..Default::default()
    };
    match inner
        .api_add_torrent(url, Some(opts))
        .await
        .context("error calling HttpApi::api_add_torrent")
    {
        Ok(response) => Ok(axum::Json(response)),
        Err(err) => Err((StatusCode::BAD_REQUEST, format!("{err:#?}"))),
    }
}

impl HttpApi {
    pub fn new(session: Arc<Session>) -> Self {
        Self {
            inner: Arc::new(ApiInternal::new(session)),
        }
    }
    pub fn add_mgr(&self, handle: TorrentManagerHandle) -> usize {
        self.inner.add_mgr(handle)
    }

    pub async fn make_http_api_and_run(self, addr: SocketAddr) -> anyhow::Result<()> {
        let state = self.inner;
        let app = Router::new()
            .route("/", routing::get(|| async move {
                axum::Json(serde_json::json!({
                    "apis": {
                        "GET /": "list all available APIs",
                        "GET /dht/stats": "DHT stats",
                        "GET /dht/table": "DHT routing table",
                        "GET /torrents": "List torrents (default torrent is 0)",
                        "GET /torrents/{index}": "Torrent details",
                        "GET /torrents/{index}/haves": "The bitfield of have pieces",
                        "GET /torrents/{index}/stats": "Torrent stats",
                        // This is kind of not secure as it just reads any local file that it has access to,
                        // or any URL, but whatever, ok for our purposes / thread model.
                        "POST /torrents": "Add a torrent here. magnet: or http:// or a local file."
                    },
                    "server": "rqbit",
                }))
            }))
            .route(
                "/dht/stats",
                routing::get({
                    let state = state.clone();
                    move || async move {
                        match state.api_dht_stats() {
                            Some(stats) => Ok(axum::Json(stats)),
                            None => Err(axum_not_found_response("DHT is disabled")),
                        }
                    }
                }),
            )
            .route(
                "/dht/table",
                routing::get({
                    let state = state.clone();
                    move || async move {
                        match state.dht.as_ref() {
                            Some(dht) => Ok(dht.with_routing_table(|r| response::Json(r.clone()))),
                            None => Err(axum_not_found_response("DHT is disabled")),
                        }
                    }
                }),
            )
            .route(
                "/torrents",
                routing::get({
                    let state = state.clone();
                    move || async move { axum::response::Json(state.api_torrent_list()) }
                }),
            )
            .route("/torrents", routing::post(post_torrent))
            .route(
                "/torrents/:id",
                routing::get({
                    let state = state.clone();
                    move |axum::extract::Path(idx): axum::extract::Path<usize>| async move {
                        axum_json_or_torrent_not_found(idx, state.api_torrent_details(idx))
                    }
                }),
            )
            .route(
                "/torrents/:id/haves",
                routing::get({
                    let state = state.clone();
                    move |axum::extract::Path(idx): axum::extract::Path<usize>| async move {
                        match state.api_dump_haves(idx) {
                            Some(haves) => Ok(haves),
                            None => Err(axum_torrent_not_found_response(idx)),
                        }
                    }
                }),
            )
            .route(
                "/torrents/:id/stats",
                routing::get({
                    let state = state.clone();
                    move |axum::extract::Path(idx): axum::extract::Path<usize>| async move {
                        axum_json_or_torrent_not_found(idx, state.api_stats(idx))
                    }
                }),
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
