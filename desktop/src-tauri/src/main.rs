// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::Context;
use http::StatusCode;
use librqbit::{
    api::{
        ApiAddTorrentResponse, EmptyJsonResponse, TorrentDetailsResponse, TorrentListResponse,
        TorrentStats,
    },
    librqbit_spawn, AddTorrent, AddTorrentOptions, Api, ApiError, Session, SessionOptions,
};
use tracing::error_span;

struct State {
    api: Api,
}

#[tauri::command]
fn torrents_list(state: tauri::State<State>) -> TorrentListResponse {
    state.api.api_torrent_list()
}

#[tauri::command]
async fn torrent_create_from_url(
    state: tauri::State<'_, State>,
    url: String,
    opts: Option<AddTorrentOptions>,
) -> Result<ApiAddTorrentResponse, ApiError> {
    state
        .api
        .api_add_torrent(AddTorrent::Url(url.into()), opts)
        .await
}

#[tauri::command]
async fn torrent_create_from_base64_file(
    state: tauri::State<'_, State>,
    contents: String,
    opts: Option<AddTorrentOptions>,
) -> Result<ApiAddTorrentResponse, ApiError> {
    use base64::{engine::general_purpose, Engine as _};
    let bytes = general_purpose::STANDARD
        .decode(&contents)
        .context("invalid base64")
        .map_err(|e| ApiError::new_from_anyhow(StatusCode::BAD_REQUEST, e))?;
    state
        .api
        .api_add_torrent(AddTorrent::TorrentFileBytes(bytes.into()), opts)
        .await
}

#[tauri::command]
async fn torrent_details(
    state: tauri::State<'_, State>,
    id: usize,
) -> Result<TorrentDetailsResponse, ApiError> {
    state.api.api_torrent_details(id)
}

#[tauri::command]
async fn torrent_stats(
    state: tauri::State<'_, State>,
    id: usize,
) -> Result<TorrentStats, ApiError> {
    state.api.api_stats_v1(id)
}

#[tauri::command]
async fn torrent_action_delete(
    state: tauri::State<'_, State>,
    id: usize,
) -> Result<EmptyJsonResponse, ApiError> {
    state.api.api_torrent_action_delete(id)
}

#[tauri::command]
async fn torrent_action_pause(
    state: tauri::State<'_, State>,
    id: usize,
) -> Result<EmptyJsonResponse, ApiError> {
    state.api.api_torrent_action_pause(id)
}

#[tauri::command]
async fn torrent_action_forget(
    state: tauri::State<'_, State>,
    id: usize,
) -> Result<EmptyJsonResponse, ApiError> {
    state.api.api_torrent_action_forget(id)
}

#[tauri::command]
async fn torrent_action_start(
    state: tauri::State<'_, State>,
    id: usize,
) -> Result<EmptyJsonResponse, ApiError> {
    state.api.api_torrent_action_start(id)
}

#[tauri::command]
fn get_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn init_logging() -> tokio::sync::mpsc::UnboundedSender<String> {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    let (stderr_filter, reload_stderr_filter) =
        tracing_subscriber::reload::Layer::new(EnvFilter::builder().parse("info").unwrap());

    let layered = tracing_subscriber::registry().with(fmt::layer().with_filter(stderr_filter));
    layered.init();

    let (reload_tx, mut reload_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    librqbit_spawn(
        "fmt_filter_reloader",
        error_span!("fmt_filter_reloader"),
        async move {
            while let Some(rust_log) = reload_rx.recv().await {
                let stderr_env_filter = match EnvFilter::builder().parse(&rust_log) {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("can't parse env filter {:?}: {:#?}", rust_log, e);
                        continue;
                    }
                };
                eprintln!("setting RUST_LOG to {:?}", rust_log);
                let _ = reload_stderr_filter.reload(stderr_env_filter);
            }
            Ok(())
        },
    );
    reload_tx
}

async fn start_session() {
    let rust_log_reload_tx = init_logging();

    tauri::async_runtime::set(tokio::runtime::Handle::current());

    let download_folder = directories::UserDirs::new()
        .expect("directories::UserDirs::new()")
        .download_dir()
        .expect("download_dir()")
        .to_path_buf();

    let session = Session::new_with_opts(
        download_folder,
        SessionOptions {
            disable_dht: false,
            disable_dht_persistence: false,
            persistence: true,
            listen_port_range: Some(4240..4260),
            ..Default::default()
        },
    )
    .await
    .expect("couldn't set up librqbit session");

    let api = Api::new(session.clone(), None);

    librqbit_spawn(
        "http api",
        error_span!("http_api"),
        librqbit::http_api::HttpApi::new(session, Some(rust_log_reload_tx))
            .make_http_api_and_run("127.0.0.1:3030".parse().unwrap(), false),
    );

    tauri::Builder::default()
        .manage(State { api })
        .invoke_handler(tauri::generate_handler![
            torrents_list,
            torrent_details,
            torrent_stats,
            torrent_create_from_url,
            torrent_action_delete,
            torrent_action_pause,
            torrent_action_forget,
            torrent_action_start,
            torrent_create_from_base64_file,
            get_version
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn main() {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("couldn't set up tokio runtime")
        .block_on(start_session())
}
