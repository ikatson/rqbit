mod configure;
mod dht;
mod logging;
mod other;
mod playlist;
mod streaming;
mod torrents;

use std::sync::Arc;

use axum::{
    response::{IntoResponse, Redirect},
    routing::{get, post},
    Router,
};
use http::request::Parts;

use super::HttpApi;
type ApiState = Arc<HttpApi>;

async fn h_api_root(parts: Parts) -> impl IntoResponse {
    // If browser, and webui enabled, redirect to web
    #[cfg(feature = "webui")]
    {
        if parts
            .headers
            .get("Accept")
            .and_then(|h| h.to_str().ok())
            .is_some_and(|h| h.contains("text/html"))
        {
            return Redirect::temporary("./web/").into_response();
        }
    }

    let json = serde_json::json!({
        "apis": {
            "GET /": "list all available APIs",
            "GET /dht/stats": "DHT stats",
            "GET /dht/table": "DHT routing table",
            "GET /torrents": "List torrents",
            "GET /torrents/playlist": "Generate M3U8 playlist for all files in all torrents",
            "GET /stats": "Global session stats",
            "POST /torrents/resolve_magnet": "Resolve a magnet to torrent file bytes",
            "GET /torrents/{id_or_infohash}": "Torrent details",
            "GET /torrents/{id_or_infohash}/metadata": "Download the corresponding torrent file",
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
    });

    ([("Content-Type", "application/json")], axum::Json(json)).into_response()
}

pub fn make_api_router(state: ApiState) -> Router {
    let mut api_router = Router::new()
        .route("/", get(h_api_root))
        .route("/stream_logs", get(logging::h_stream_logs))
        .route("/rust_log", post(logging::h_set_rust_log))
        .route("/dht/stats", get(dht::h_dht_stats))
        .route("/dht/table", get(dht::h_dht_table))
        .route("/stats", get(torrents::h_session_stats))
        .route("/torrents", get(torrents::h_torrents_list))
        .route("/torrents/{id}", get(torrents::h_torrent_details))
        .route("/torrents/{id}/haves", get(torrents::h_torrent_haves))
        .route("/torrents/{id}/metadata", get(torrents::h_metadata))
        .route("/torrents/{id}/stats", get(torrents::h_torrent_stats_v0))
        .route("/torrents/{id}/stats/v1", get(torrents::h_torrent_stats_v1))
        .route("/torrents/{id}/peer_stats", get(torrents::h_peer_stats))
        .route("/torrents/{id}/playlist", get(playlist::h_torrent_playlist))
        .route("/torrents/playlist", get(playlist::h_global_playlist))
        .route("/torrents/resolve_magnet", post(other::h_resolve_magnet))
        .route(
            "/torrents/{id}/stream/{file_id}",
            get(streaming::h_torrent_stream_file),
        )
        .route(
            "/torrents/{id}/stream/{file_id}/{*filename}",
            get(streaming::h_torrent_stream_file),
        );

    if !state.opts.read_only {
        api_router = api_router
            .route("/torrents", post(torrents::h_torrents_post))
            .route(
                "/torrents/limits",
                post(configure::h_update_session_ratelimits),
            )
            .route(
                "/torrents/{id}/pause",
                post(torrents::h_torrent_action_pause),
            )
            .route(
                "/torrents/{id}/start",
                post(torrents::h_torrent_action_start),
            )
            .route(
                "/torrents/{id}/forget",
                post(torrents::h_torrent_action_forget),
            )
            .route(
                "/torrents/{id}/delete",
                post(torrents::h_torrent_action_delete),
            )
            .route(
                "/torrents/{id}/update_only_files",
                post(torrents::h_torrent_action_update_only_files),
            )
            .route("/torrents/{id}/add_peers", post(torrents::h_add_peers));
    }

    api_router.with_state(state)
}
