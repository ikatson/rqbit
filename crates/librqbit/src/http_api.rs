use anyhow::Context;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use bencode::AsDisplay;
use buffers::ByteBuf;
use futures::future::BoxFuture;
use futures::{FutureExt, TryStreamExt};
use http::{HeaderMap, HeaderValue, StatusCode};
use itertools::Itertools;

use serde::{Deserialize, Serialize};
use std::io::SeekFrom;
use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;
use tokio::io::AsyncSeekExt;
use tokio::net::TcpListener;
use tower_http::trace::{DefaultOnFailure, DefaultOnResponse, OnFailure};
use tracing::{debug, error_span, trace, Span};

use axum::Router;

use crate::api::{Api, TorrentIdOrHash};
use crate::peer_connection::PeerConnectionOptions;
use crate::session::{AddTorrent, AddTorrentOptions, SUPPORTED_SCHEMES};
use crate::torrent_state::peer::stats::snapshot::PeerStatsFilter;

type ApiState = Api;

use crate::api::Result;
use crate::{ApiError, ListOnlyResponse, ManagedTorrent};

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
    pub fn make_http_api_and_run(
        self,
        listener: TcpListener,
        upnp_router: Option<Router>,
    ) -> BoxFuture<'static, anyhow::Result<()>> {
        let state = self.inner;

        async fn api_root() -> impl IntoResponse {
            axum::Json(serde_json::json!({
                "apis": {
                    "GET /": "list all available APIs",
                    "GET /dht/stats": "DHT stats",
                    "GET /dht/table": "DHT routing table",
                    "GET /torrents": "List torrents",
                    "GET /torrents/playlist": "Generate M3U8 playlist for all files in all torrents",
                    "GET /stats": "Global session stats",
                    "POST /torrents/resolve_magnet": "Resolve a magnet to torrent file bytes",
                    "GET /torrents/{id_or_infohash}": "Torrent details",
                    "GET /torrents/{id_or_infohash}/haves": "The bitfield of have pieces",
                    "GET /torrents/{id_or_infohash}/playlist": "Generate M3U8 playlist for this torrent",
                    "GET /torrents/{id_or_infohash}/stats/v1": "Torrent stats",
                    "GET /torrents/{id_or_infohash}/peer_stats": "Per peer stats",
                    "GET /torrents/{id_or_infohash}/stream/{file_idx}": "Stream a file. Accepts Range header to seek.",
                    "POST /torrents/{id_or_infohash}/pause": "Pause torrent",
                    "POST /torrents/{id_or_infohash}/start": "Resume torrent",
                    "POST /torrents/{id_or_infohash}/forget": "Forget about the torrent, keep the files",
                    "POST /torrents/{id_or_infohash}/delete": "Forget about the torrent, remove the files",
                    "POST /torrents/{id_or_infohash}/update_only_files": "Change the selection of files to download. You need to POST json of the following form {\"only_files\": [0, 1, 2]}",
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

        async fn session_stats(State(state): State<ApiState>) -> impl IntoResponse {
            axum::Json(state.api_session_stats())
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
            Path(idx): Path<TorrentIdOrHash>,
        ) -> Result<impl IntoResponse> {
            state.api_torrent_details(idx).map(axum::Json)
        }

        fn torrent_playlist_items(handle: &ManagedTorrent) -> Result<Vec<(usize, String)>> {
            let mut playlist_items = handle
                .shared()
                .info
                .iter_filenames_and_lengths()?
                .enumerate()
                .filter_map(|(file_idx, (filename, _))| {
                    let filename = filename.to_vec().ok()?.join("/");
                    let is_playable = mime_guess::from_path(&filename)
                        .first()
                        .map(|mime| {
                            mime.type_() == mime_guess::mime::VIDEO
                                || mime.type_() == mime_guess::mime::AUDIO
                        })
                        .unwrap_or(false);
                    if is_playable {
                        let filename = urlencoding::encode(&filename);
                        Some((file_idx, filename.into_owned()))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            playlist_items.sort_by(|left, right| left.1.cmp(&right.1));
            Ok(playlist_items)
        }

        fn get_host(headers: &HeaderMap) -> Result<&str> {
            Ok(headers
                .get("host")
                .ok_or_else(|| {
                    ApiError::new_from_text(StatusCode::BAD_REQUEST, "Missing host header")
                })?
                .to_str()
                .context("hostname is not string")?)
        }

        fn build_playlist_content(
            host: &str,
            it: impl IntoIterator<Item = (TorrentIdOrHash, usize, String)>,
        ) -> impl IntoResponse {
            let body = it
                .into_iter()
                .map(|(torrent_idx, file_idx, filename)| {
                    format!("http://{host}/torrents/{torrent_idx}/stream/{file_idx}/{filename}")
                })
                .join("\r\n");
            (
                [
                    ("Content-Type", "application/mpegurl; charset=utf-8"),
                    (
                        "Content-Disposition",
                        "attachment; filename=\"rqbit-playlist.m3u8\"",
                    ),
                ],
                body,
            )
        }

        async fn resolve_magnet(
            State(state): State<ApiState>,
            inp_headers: HeaderMap,
            url: String,
        ) -> Result<impl IntoResponse> {
            let added = state
                .session()
                .add_torrent(
                    AddTorrent::from_url(&url),
                    Some(AddTorrentOptions {
                        list_only: true,
                        ..Default::default()
                    }),
                )
                .await?;
            let (info, content) = match added {
                crate::AddTorrentResponse::AlreadyManaged(_, handle) => (
                    handle.shared().info.clone(),
                    handle.shared().torrent_bytes.clone(),
                ),
                crate::AddTorrentResponse::ListOnly(ListOnlyResponse {
                    info,
                    torrent_bytes,
                    ..
                }) => (info, torrent_bytes),
                crate::AddTorrentResponse::Added(_, _) => {
                    return Err(ApiError::new_from_text(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "bug: torrent was added to session, but shouldn't have been",
                    ))
                }
            };

            let mut headers = HeaderMap::new();

            if inp_headers
                .get("Accept")
                .and_then(|v| std::str::from_utf8(v.as_bytes()).ok())
                == Some("application/json")
            {
                let data = bencode::dyn_from_bytes::<AsDisplay<ByteBuf>>(&content)
                    .context("error decoding .torrent file content")?;
                let data = serde_json::to_string(&data).context("error serializing")?;
                headers.insert("Content-Type", HeaderValue::from_static("application/json"));
                return Ok((headers, data).into_response());
            }

            headers.insert(
                "Content-Type",
                HeaderValue::from_static("application/x-bittorrent"),
            );

            if let Some(name) = info.name.as_ref() {
                if let Ok(name) = std::str::from_utf8(name) {
                    if let Ok(h) =
                        HeaderValue::from_str(&format!("attachment; filename=\"{}.torrent\"", name))
                    {
                        headers.insert("Content-Disposition", h);
                    }
                }
            }
            Ok((headers, content).into_response())
        }

        async fn torrent_playlist(
            State(state): State<ApiState>,
            headers: HeaderMap,
            Path(idx): Path<TorrentIdOrHash>,
        ) -> Result<impl IntoResponse> {
            let host = get_host(&headers)?;
            let playlist_items = torrent_playlist_items(&*state.mgr_handle(idx)?)?;
            Ok(build_playlist_content(
                host,
                playlist_items
                    .into_iter()
                    .map(move |(file_idx, filename)| (idx, file_idx, filename)),
            ))
        }

        async fn global_playlist(
            State(state): State<ApiState>,
            headers: HeaderMap,
        ) -> Result<impl IntoResponse> {
            let host = get_host(&headers)?;
            let all_items = state.session().with_torrents(|torrents| {
                torrents
                    .filter_map(|(torrent_idx, handle)| {
                        torrent_playlist_items(handle)
                            .map(move |items| {
                                items.into_iter().map(move |(file_idx, filename)| {
                                    (torrent_idx.into(), file_idx, filename)
                                })
                            })
                            .ok()
                    })
                    .flatten()
                    .collect::<Vec<_>>()
            });
            Ok(build_playlist_content(host, all_items))
        }

        async fn torrent_haves(
            State(state): State<ApiState>,
            Path(idx): Path<TorrentIdOrHash>,
        ) -> Result<impl IntoResponse> {
            state.api_dump_haves(idx)
        }

        async fn torrent_stats_v0(
            State(state): State<ApiState>,
            Path(idx): Path<TorrentIdOrHash>,
        ) -> Result<impl IntoResponse> {
            state.api_stats_v0(idx).map(axum::Json)
        }

        async fn torrent_stats_v1(
            State(state): State<ApiState>,
            Path(idx): Path<TorrentIdOrHash>,
        ) -> Result<impl IntoResponse> {
            state.api_stats_v1(idx).map(axum::Json)
        }

        async fn peer_stats(
            State(state): State<ApiState>,
            Path(idx): Path<TorrentIdOrHash>,
            Query(filter): Query<PeerStatsFilter>,
        ) -> Result<impl IntoResponse> {
            state.api_peer_stats(idx, filter).map(axum::Json)
        }

        async fn torrent_stream_file(
            State(state): State<ApiState>,
            Path((idx, file_id)): Path<(TorrentIdOrHash, usize)>,
            headers: http::HeaderMap,
        ) -> Result<impl IntoResponse> {
            let mut stream = state.api_stream(idx, file_id)?;
            let mut status = StatusCode::OK;
            let mut output_headers = HeaderMap::new();
            output_headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));

            if let Ok(mime) = state.torrent_file_mime_type(idx, file_id) {
                output_headers.insert(
                    http::header::CONTENT_TYPE,
                    HeaderValue::from_str(mime).context("bug - invalid MIME")?,
                );
            }

            let range_header = headers.get(http::header::RANGE);
            trace!(torrent_id=%idx, file_id=file_id, range=?range_header, "request for HTTP stream");

            if let Some(range) = range_header {
                let offset: Option<u64> = range
                    .to_str()
                    .ok()
                    .and_then(|s| s.strip_prefix("bytes="))
                    .and_then(|s| s.strip_suffix('-'))
                    .and_then(|s| s.parse().ok());
                if let Some(offset) = offset {
                    status = StatusCode::PARTIAL_CONTENT;
                    stream
                        .seek(SeekFrom::Start(offset))
                        .await
                        .context("error seeking")?;

                    output_headers.insert(
                        http::header::CONTENT_LENGTH,
                        HeaderValue::from_str(&format!("{}", stream.len() - stream.position()))
                            .context("bug")?,
                    );
                    output_headers.insert(
                        http::header::CONTENT_RANGE,
                        HeaderValue::from_str(&format!(
                            "bytes {}-{}/{}",
                            stream.position(),
                            stream.len().saturating_sub(1),
                            stream.len()
                        ))
                        .context("bug")?,
                    );
                }
            } else {
                output_headers.insert(
                    http::header::CONTENT_LENGTH,
                    HeaderValue::from_str(&format!("{}", stream.len())).context("bug")?,
                );
            }

            let s = tokio_util::io::ReaderStream::new(stream);
            Ok((status, (output_headers, axum::body::Body::from_stream(s))))
        }

        async fn torrent_action_pause(
            State(state): State<ApiState>,
            Path(idx): Path<TorrentIdOrHash>,
        ) -> Result<impl IntoResponse> {
            state.api_torrent_action_pause(idx).await.map(axum::Json)
        }

        async fn torrent_action_start(
            State(state): State<ApiState>,
            Path(idx): Path<TorrentIdOrHash>,
        ) -> Result<impl IntoResponse> {
            state.api_torrent_action_start(idx).await.map(axum::Json)
        }

        async fn torrent_action_forget(
            State(state): State<ApiState>,
            Path(idx): Path<TorrentIdOrHash>,
        ) -> Result<impl IntoResponse> {
            state.api_torrent_action_forget(idx).await.map(axum::Json)
        }

        async fn torrent_action_delete(
            State(state): State<ApiState>,
            Path(idx): Path<TorrentIdOrHash>,
        ) -> Result<impl IntoResponse> {
            state.api_torrent_action_delete(idx).await.map(axum::Json)
        }

        #[derive(Deserialize)]
        struct UpdateOnlyFilesRequest {
            only_files: Vec<usize>,
        }

        async fn torrent_action_update_only_files(
            State(state): State<ApiState>,
            Path(idx): Path<TorrentIdOrHash>,
            axum::Json(req): axum::Json<UpdateOnlyFilesRequest>,
        ) -> Result<impl IntoResponse> {
            state
                .api_torrent_action_update_only_files(idx, &req.only_files.into_iter().collect())
                .await
                .map(axum::Json)
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
            .route("/stats", get(session_stats))
            .route("/torrents", get(torrents_list))
            .route("/torrents/:id", get(torrent_details))
            .route("/torrents/:id/haves", get(torrent_haves))
            .route("/torrents/:id/stats", get(torrent_stats_v0))
            .route("/torrents/:id/stats/v1", get(torrent_stats_v1))
            .route("/torrents/:id/peer_stats", get(peer_stats))
            .route("/torrents/:id/stream/:file_id", get(torrent_stream_file))
            .route("/torrents/:id/playlist", get(torrent_playlist))
            .route("/torrents/playlist", get(global_playlist))
            .route("/torrents/resolve_magnet", post(resolve_magnet))
            .route(
                "/torrents/:id/stream/:file_id/*filename",
                get(torrent_stream_file),
            );

        if !self.opts.read_only {
            app = app
                .route("/torrents", post(torrents_post))
                .route("/torrents/:id/pause", post(torrent_action_pause))
                .route("/torrents/:id/start", post(torrent_action_start))
                .route("/torrents/:id/forget", post(torrent_action_forget))
                .route("/torrents/:id/delete", post(torrent_action_delete))
                .route(
                    "/torrents/:id/update_only_files",
                    post(torrent_action_update_only_files),
                );
        }

        #[cfg(feature = "webui")]
        {
            use axum::response::Redirect;

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
            app = app.route("/web", get(|| async { Redirect::permanent("/web/") }))
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

            let allow_regex = std::env::var("CORS_ALLOW_REGEXP")
                .ok()
                .and_then(|value| regex::bytes::Regex::new(&value).ok());

            tower_http::cors::CorsLayer::default()
                .allow_origin(AllowOrigin::predicate(move |v, _| {
                    ALLOWED_ORIGINS.contains(&v.as_bytes())
                        || allow_regex
                            .as_ref()
                            .map(move |r| r.is_match(v.as_bytes()))
                            .unwrap_or(false)
                }))
                .allow_headers(AllowHeaders::any())
        };

        let mut app = app.with_state(state);

        if let Some(upnp_router) = upnp_router {
            app = app.nest("/upnp", upnp_router);
        }

        let app = app
            .layer(cors_layer)
            .layer(
                tower_http::trace::TraceLayer::new_for_http()
                    .make_span_with(|req: &Request| {
                        let method = req.method();
                        let uri = req.uri();
                        if let Some(ConnectInfo(addr)) =
                            req.extensions().get::<ConnectInfo<SocketAddr>>()
                        {
                            error_span!("request", %method, %uri, %addr)
                        } else {
                            error_span!("request", %method, %uri)
                        }
                    })
                    .on_request(|req: &Request, _: &Span| {
                        if req.uri().path().starts_with("/upnp") {
                            debug!(headers=?req.headers())
                        }
                    })
                    .on_response(DefaultOnResponse::new().include_headers(true))
                    .on_failure({
                        let mut default = DefaultOnFailure::new();
                        move |failure_class, latency, span: &Span| match failure_class {
                            tower_http::classify::ServerErrorsFailureClass::StatusCode(
                                StatusCode::NOT_IMPLEMENTED,
                            ) => {}
                            _ => default.on_failure(failure_class, latency, span),
                        }
                    }),
            )
            .into_make_service_with_connect_info::<SocketAddr>();

        async move {
            axum::serve(listener, app)
                .await
                .context("error running HTTP API")
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
