// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use http::StatusCode;
use librqbit::{
    api::{
        Api, ApiAddTorrentResponse, EmptyJsonResponse, TorrentDetailsResponse, TorrentListResponse,
    },
    api_error::ApiError,
    session::AddTorrentOptions,
    torrent_state::stats::TorrentStats,
};

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
        .api_add_torrent(librqbit::session::AddTorrent::Url(url.into()), opts)
        .await
}

#[tauri::command]
async fn torrent_create_from_base64_file(
    state: tauri::State<'_, State>,
    contents: String,
    opts: Option<AddTorrentOptions>,
) -> Result<ApiAddTorrentResponse, ApiError> {
    use base64::{engine::general_purpose, Engine as _};
    let bytes = general_purpose::STANDARD_NO_PAD
        .decode(&contents)
        .map_err(|_| ApiError::new_from_string(StatusCode::BAD_REQUEST, "invalid base64".into()))?;
    state
        .api
        .api_add_torrent(
            librqbit::session::AddTorrent::TorrentFileBytes(bytes.into()),
            opts,
        )
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

async fn start_session() {
    tauri::async_runtime::set(tokio::runtime::Handle::current());

    let download_folder = directories::UserDirs::new()
        .expect("directories::UserDirs::new()")
        .download_dir()
        .expect("download_dir()")
        .to_path_buf();

    let s = librqbit::session::Session::new_with_opts(
        download_folder,
        Default::default(),
        librqbit::session::SessionOptions {
            disable_dht: false,
            disable_dht_persistence: false,
            persistence: true,
            ..Default::default()
        },
    )
    .await
    .expect("couldn't set up librqbit session");

    let api = Api::new(s, None);

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
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn main() {
    tracing_subscriber::fmt::init();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("couldn't set up tokio runtime")
        .block_on(start_session())
}
