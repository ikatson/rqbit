use anyhow::Context;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use futures::future::BoxFuture;
use futures::{FutureExt, TryStreamExt};
use itertools::Itertools;

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;
use tracing::{debug, info};

use axum::Router;

use crate::api::Api;
use crate::peer_connection::PeerConnectionOptions;
use crate::session::{AddTorrent, AddTorrentOptions, SUPPORTED_SCHEMES};
use crate::torrent_state::peer::stats::snapshot::PeerStatsFilter;

type ApiState = Api;

use crate::api::Result;

/// An HTTP server for the API.
pub struct HttpApi {
    inner: ApiState,
    opts: HttpApiOptions,
}

#[derive(Debug, Default)]
pub struct HttpApiOptions {
    pub read_only: bool,
}

impl HttpApi {
    pub fn new(api: Api, opts: Option<HttpApiOptions>) -> Self {
        Self {
            inner: api,
            opts: opts.unwrap_or_default(),
        }
    }

    /// Run the HTTP server forever on the given address.
    /// If read_only is passed, no state-modifying methods will be exposed.
    #[inline(never)]
    pub fn make_http_api_and_run(self, addr: SocketAddr) -> BoxFuture<'static, anyhow::Result<()>> {
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
                "version": env!("CARGO_PKG_VERSION"),
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
            let is_url = params.is_url;
            let opts = params.into_add_torrent_options();
            let data = data.to_vec();
            let add = match is_url {
                Some(true) => AddTorrent::Url(
                    String::from_utf8(data)
                        .context("invalid utf-8 for passed URL")?
                        .into(),
                ),
                Some(false) => AddTorrent::TorrentFileBytes(data.into()),

                // Guess the format.
                None if SUPPORTED_SCHEMES
                    .iter()
                    .any(|s| data.starts_with(s.as_bytes())) =>
                {
                    AddTorrent::Url(
                        String::from_utf8(data)
                            .context("invalid utf-8 for passed URL")?
                            .into(),
                    )
                }
                _ => AddTorrent::TorrentFileBytes(data.into()),
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

        async fn stream_logs(State(state): State<ApiState>) -> Result<impl IntoResponse> {
            let s = state.api_log_lines_stream()?.map_err(|e| {
                debug!(error=%e, "stream_logs");
                e
            });
            Ok(axum::body::Body::from_stream(s))
        }

        let mut app = Router::new()
            .route("/", get(api_root))
            .route("/stream_logs", get(stream_logs))
            .route("/rust_log", post(set_rust_log))
            .route("/dht/stats", get(dht_stats))
            .route("/dht/table", get(dht_table))
            .route("/torrents", get(torrents_list))
            .route("/torrents/:id", get(torrent_details))
            .route("/torrents/:id/haves", get(torrent_haves))
            .route("/torrents/:id/stats", get(torrent_stats_v0))
            .route("/torrents/:id/stats/v1", get(torrent_stats_v1))
            .route("/torrents/:id/peer_stats", get(peer_stats));

        if !self.opts.read_only {
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
                    "/assets/index.css",
                    get(|| async {
                        (
                            [("Content-Type", "text/css")],
                            include_str!("../webui/dist/assets/index.css"),
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

            app = app.nest("/web/", webui_router);
        }

        let cors_layer = {
            use tower_http::cors::{AllowHeaders, AllowOrigin};

            const ALLOWED_ORIGINS: [&[u8]; 4] = [
                // Webui-dev
                b"http://localhost:3031",
                b"http://127.0.0.1:3031",
                // Tauri dev
                b"http://localhost:1420",
                // Tauri prod
                b"tauri://localhost",
            ];

            tower_http::cors::CorsLayer::default()
                .allow_origin(AllowOrigin::predicate(|v, _| {
                    ALLOWED_ORIGINS.contains(&v.as_bytes())
                }))
                .allow_headers(AllowHeaders::any())
        };

        let app = app
            .layer(cors_layer)
            .layer(tower_http::trace::TraceLayer::new_for_http())
            .with_state(state)
            .into_make_service();

        info!(%addr, "starting HTTP server");

        use tokio::net::TcpListener;

        async move {
            let listener = TcpListener::bind(&addr)
                .await
                .with_context(|| format!("error binding to {addr}"))?;
            axum::serve(listener, app).await?;
            Ok(())
        }
        .boxed()
    }
}

pub(crate) struct OnlyFiles(Vec<usize>);
pub(crate) struct InitialPeers(pub Vec<SocketAddr>);

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct TorrentAddQueryParams {
    pub overwrite: Option<bool>,
    pub output_folder: Option<String>,
    pub sub_folder: Option<String>,
    pub only_files_regex: Option<String>,
    pub only_files: Option<OnlyFiles>,
    pub peer_connect_timeout: Option<u64>,
    pub peer_read_write_timeout: Option<u64>,
    pub initial_peers: Option<InitialPeers>,
    // Will force interpreting the content as a URL.
    pub is_url: Option<bool>,
    pub list_only: Option<bool>,
}

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

impl<'de> Deserialize<'de> for InitialPeers {
    fn deserialize<D>(deserializer: D) -> std::prelude::v1::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let string = String::deserialize(deserializer)?;
        let mut addrs = Vec::new();
        for addr_str in string.split(',').filter(|s| !s.is_empty()) {
            addrs.push(SocketAddr::from_str(addr_str).map_err(D::Error::custom)?);
        }
        Ok(InitialPeers(addrs))
    }
}

impl Serialize for InitialPeers {
    fn serialize<S>(&self, serializer: S) -> std::prelude::v1::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0
            .iter()
            .map(|s| s.to_string())
            .join(",")
            .serialize(serializer)
    }
}

impl TorrentAddQueryParams {
    pub fn into_add_torrent_options(self) -> AddTorrentOptions {
        AddTorrentOptions {
            overwrite: self.overwrite.unwrap_or(false),
            only_files_regex: self.only_files_regex,
            only_files: self.only_files.map(|o| o.0),
            output_folder: self.output_folder,
            sub_folder: self.sub_folder,
            list_only: self.list_only.unwrap_or(false),
            initial_peers: self.initial_peers.map(|i| i.0),
            peer_opts: Some(PeerConnectionOptions {
                connect_timeout: self.peer_connect_timeout.map(Duration::from_secs),
                read_write_timeout: self.peer_read_write_timeout.map(Duration::from_secs),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}
