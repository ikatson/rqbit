use anyhow::Context;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::routing::get;
use buffers::ByteString;
use dht::{Dht, DhtStats};
use http::StatusCode;
use librqbit_core::id20::Id20;
use librqbit_core::torrent_metainfo::TorrentMetaV1Info;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower_http::cors::{AllowHeaders, AllowOrigin};
use tracing::{info, warn};

use axum::Router;

use crate::http_api_error::{ApiError, ApiErrorExt};
use crate::peer_state::PeerStatsFilter;
use crate::session::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ListOnlyResponse, Session,
};
use crate::torrent_manager::TorrentManagerHandle;
use crate::torrent_state::StatsSnapshot;

// Public API
#[derive(Clone)]
pub struct HttpApi {
    inner: Arc<ApiInternal>,
}

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

        async fn api_root() -> impl IntoResponse {
            axum::Json(serde_json::json!({
                "apis": {
                    "GET /": "list all available APIs",
                    "GET /dht/stats": "DHT stats",
                    "GET /dht/table": "DHT routing table",
                    "GET /torrents": "List torrents (default torrent is 0)",
                    "GET /torrents/{index}": "Torrent details",
                    "GET /torrents/{index}/haves": "The bitfield of have pieces",
                    "GET /torrents/{index}/stats": "Torrent stats",
                    "GET /torrents/{index}/peer_stats": "Per peer stats",
                    // This is kind of not secure as it just reads any local file that it has access to,
                    // or any URL, but whatever, ok for our purposes / threat model.
                    "POST /torrents": "Add a torrent here. magnet: or http:// or a local file."
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
                Ok(s) => AddTorrent::from(s),
                Err(e) => AddTorrent::from(e.into_bytes()),
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

        async fn torrent_stats(
            State(state): State<ApiState>,
            Path(idx): Path<usize>,
        ) -> Result<impl IntoResponse> {
            state.api_stats(idx).map(axum::Json)
        }

        async fn peer_stats(
            State(state): State<ApiState>,
            Path(idx): Path<usize>,
            Query(filter): Query<PeerStatsFilter>,
        ) -> Result<impl IntoResponse> {
            state.api_peer_stats(idx, filter).map(axum::Json)
        }

        let app = Router::new()
            .route("/", get(api_root))
            .route("/dht/stats", get(dht_stats))
            .route("/dht/table", get(dht_table))
            .route("/torrents", get(torrents_list).post(torrents_post))
            .route("/torrents/:id", get(torrent_details))
            .route("/torrents/:id/haves", get(torrent_haves))
            .route("/torrents/:id/stats", get(torrent_stats))
            .route("/torrents/:id/peer_stats", get(peer_stats))
            .layer(
                tower_http::cors::CorsLayer::default()
                    .allow_origin(AllowOrigin::predicate(|_, _| true))
                    .allow_headers(AllowHeaders::any()),
            )
            .layer(tower_http::trace::TraceLayer::new_for_http())
            .with_state(state);

        info!("starting HTTP server on {}", addr);
        axum::Server::try_bind(&addr)
            .with_context(|| format!("error binding to {addr}"))?
            .serve(app.into_make_service())
            .await?;
        Ok(())
    }
}

type Result<T> = std::result::Result<T, ApiError>;

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

struct DurationWithHumanReadable(Duration);

impl Serialize for DurationWithHumanReadable {
    fn serialize<S>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct Tmp {
            duration: Duration,
            human_readable: String,
        }
        Tmp {
            duration: self.0,
            human_readable: format!("{:?}", self.0),
        }
        .serialize(serializer)
    }
}

#[derive(Serialize)]
struct StatsResponse {
    snapshot: StatsSnapshot,
    average_piece_download_time: Option<Duration>,
    download_speed: Speed,
    all_time_download_speed: Speed,
    time_remaining: Option<DurationWithHumanReadable>,
}

#[derive(Serialize, Deserialize)]
pub struct ApiAddTorrentResponse {
    pub id: Option<usize>,
    pub details: TorrentDetailsResponse,
}

#[derive(Serialize, Deserialize)]
pub struct TorrentAddQueryParams {
    pub overwrite: Option<bool>,
    pub output_folder: Option<String>,
    pub sub_folder: Option<String>,
    pub only_files_regex: Option<String>,
    pub list_only: Option<bool>,
}

impl TorrentAddQueryParams {
    fn into_add_torrent_options(self) -> AddTorrentOptions {
        AddTorrentOptions {
            overwrite: self.overwrite.unwrap_or(false),
            only_files_regex: self.only_files_regex,
            output_folder: self.output_folder,
            sub_folder: self.sub_folder,
            list_only: self.list_only.unwrap_or(false),
            ..Default::default()
        }
    }
}

// Private HTTP API internals. Agnostic of web framework.
pub struct ApiInternal {
    dht: Option<Dht>,
    startup_time: Instant,
    torrent_managers: RwLock<Vec<TorrentManagerHandle>>,
    session: Arc<Session>,
}

type ApiState = Arc<ApiInternal>;

impl ApiInternal {
    pub fn new(session: Arc<Session>) -> Self {
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

    fn api_peer_stats(
        &self,
        idx: usize,
        filter: PeerStatsFilter,
    ) -> Result<crate::peer_state::PeerStatsSnapshot> {
        let handle = self.mgr_handle(idx)?;
        Ok(handle.torrent_state().per_peer_stats_snapshot(filter))
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
            AddTorrentResponse::AlreadyManaged(managed) => {
                return Err(anyhow::anyhow!(
                    "{:?} is already managed, downloaded to {:?}",
                    managed.info_hash,
                    managed.output_folder
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

    fn api_dht_stats(&self) -> Result<DhtStats> {
        self.dht
            .as_ref()
            .map(|d| d.stats())
            .ok_or(ApiError::dht_disabled())
    }

    fn api_dht_table(&self) -> Result<impl Serialize> {
        let dht = self.dht.as_ref().ok_or(ApiError::dht_disabled())?;
        Ok(dht.with_routing_table(|r| r.clone()))
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
            time_remaining: estimator.time_remaining().map(DurationWithHumanReadable),
        })
    }

    fn api_dump_haves(&self, idx: usize) -> Result<String> {
        let mgr = self.mgr_handle(idx)?;
        Ok(format!(
            "{:?}",
            mgr.torrent_state()
                .lock_read("api_dump_haves")
                .chunks
                .get_have_pieces(),
        ))
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
