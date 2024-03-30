// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;

use std::{
    fs::{File, OpenOptions},
    io::{BufReader, BufWriter},
    path::Path,
    sync::Arc,
};

use anyhow::Context;
use config::RqbitDesktopConfig;
use http::StatusCode;
use librqbit::{
    api::{
        ApiAddTorrentResponse, EmptyJsonResponse, TorrentDetailsResponse, TorrentListResponse,
        TorrentStats,
    },
    dht::PersistentDhtConfig,
    tracing_subscriber_config_utils::{init_logging, InitLoggingOptions, InitLoggingResult},
    AddTorrent, AddTorrentOptions, Api, ApiError, PeerConnectionOptions, Session, SessionOptions,
};
use parking_lot::RwLock;
use serde::Serialize;
use tracing::{error, error_span, info, warn};

const ERR_NOT_CONFIGURED: ApiError =
    ApiError::new_from_text(StatusCode::FAILED_DEPENDENCY, "not configured");

struct StateShared {
    config: config::RqbitDesktopConfig,
    api: Option<Api>,
}

struct State {
    config_filename: String,
    shared: Arc<RwLock<Option<StateShared>>>,
    init_logging: InitLoggingResult,
}

fn read_config(path: &str) -> anyhow::Result<RqbitDesktopConfig> {
    let rdr = BufReader::new(File::open(path)?);
    Ok(serde_json::from_reader(rdr)?)
}

fn write_config(path: &str, config: &RqbitDesktopConfig) -> anyhow::Result<()> {
    std::fs::create_dir_all(Path::new(path).parent().context("no parent")?)
        .context("error creating dirs")?;
    let tmp = format!("{}.tmp", path);
    let mut tmp_file = BufWriter::new(
        OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(&tmp)?,
    );
    serde_json::to_writer(&mut tmp_file, config)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

async fn api_from_config(
    init_logging: &InitLoggingResult,
    config: &RqbitDesktopConfig,
) -> anyhow::Result<Api> {
    let session = Session::new_with_opts(
        config.default_download_location.clone(),
        SessionOptions {
            disable_dht: config.dht.disable,
            disable_dht_persistence: config.dht.disable_persistence,
            dht_config: Some(PersistentDhtConfig {
                config_filename: Some(config.dht.persistence_filename.clone()),
                ..Default::default()
            }),
            persistence: !config.persistence.disable,
            persistence_filename: Some(config.persistence.filename.clone()),
            peer_opts: Some(PeerConnectionOptions {
                connect_timeout: Some(config.peer_opts.connect_timeout),
                read_write_timeout: Some(config.peer_opts.read_write_timeout),
                ..Default::default()
            }),
            listen_port_range: if !config.tcp_listen.disable {
                Some(config.tcp_listen.min_port..config.tcp_listen.max_port)
            } else {
                None
            },
            enable_upnp_port_forwarding: !config.upnp.disable,
            ..Default::default()
        },
    )
    .await
    .context("couldn't set up librqbit session")?;

    let api = Api::new(
        session.clone(),
        Some(init_logging.rust_log_reload_tx.clone()),
        Some(init_logging.line_broadcast.clone()),
    );

    if !config.http_api.disable {
        let http_api_task = librqbit::http_api::HttpApi::new(
            api.clone(),
            Some(librqbit::http_api::HttpApiOptions {
                read_only: config.http_api.read_only,
            }),
        )
        .make_http_api_and_run(config.http_api.listen_addr);

        session.spawn(error_span!("http_api"), http_api_task);
    }
    Ok(api)
}

impl State {
    async fn new(init_logging: InitLoggingResult) -> Self {
        let config_filename = directories::ProjectDirs::from("com", "rqbit", "desktop")
            .expect("directories::ProjectDirs::from")
            .config_dir()
            .join("config.json")
            .to_str()
            .expect("to_str()")
            .to_owned();

        if let Ok(config) = read_config(&config_filename) {
            let api = api_from_config(&init_logging, &config).await.ok();
            let shared = Arc::new(RwLock::new(Some(StateShared { config, api })));

            return Self {
                config_filename,
                shared,
                init_logging,
            };
        }

        Self {
            config_filename,
            init_logging,
            shared: Arc::new(RwLock::new(None)),
        }
    }

    fn api(&self) -> Result<Api, ApiError> {
        let g = self.shared.read();
        match g.as_ref().and_then(|s| s.api.as_ref()) {
            Some(api) => Ok(api.clone()),
            None => Err(ERR_NOT_CONFIGURED),
        }
    }

    async fn configure(&self, config: RqbitDesktopConfig) -> Result<(), ApiError> {
        {
            let g = self.shared.read();
            if let Some(shared) = g.as_ref() {
                if shared.api.is_some() && shared.config == config {
                    // The config didn't change, and the API is running, nothing to do.
                    return Ok(());
                }
            }
        }

        let existing = self.shared.write().as_mut().and_then(|s| s.api.take());

        if let Some(api) = existing {
            api.session().stop().await;
        }

        let api = api_from_config(&self.init_logging, &config).await?;
        if let Err(e) = write_config(&self.config_filename, &config) {
            error!("error writing config: {:#}", e);
        }

        let mut g = self.shared.write();
        *g = Some(StateShared {
            config,
            api: Some(api),
        });
        Ok(())
    }
}

#[derive(Default, Serialize)]
struct CurrentState {
    config: Option<RqbitDesktopConfig>,
    configured: bool,
}

#[tauri::command]
fn config_default() -> config::RqbitDesktopConfig {
    config::RqbitDesktopConfig::default()
}

#[tauri::command]
fn config_current(state: tauri::State<'_, State>) -> CurrentState {
    let g = state.shared.read();
    match &*g {
        Some(s) => CurrentState {
            config: Some(s.config.clone()),
            configured: s.api.is_some(),
        },
        None => Default::default(),
    }
}

#[tauri::command]
async fn config_change(
    state: tauri::State<'_, State>,
    config: RqbitDesktopConfig,
) -> Result<EmptyJsonResponse, ApiError> {
    state.configure(config).await.map(|_| EmptyJsonResponse {})
}

#[tauri::command]
fn torrents_list(state: tauri::State<State>) -> Result<TorrentListResponse, ApiError> {
    Ok(state.api()?.api_torrent_list())
}

#[tauri::command]
async fn torrent_create_from_url(
    state: tauri::State<'_, State>,
    url: String,
    opts: Option<AddTorrentOptions>,
) -> Result<ApiAddTorrentResponse, ApiError> {
    state
        .api()?
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
        .api()?
        .api_add_torrent(AddTorrent::TorrentFileBytes(bytes.into()), opts)
        .await
}

#[tauri::command]
async fn torrent_details(
    state: tauri::State<'_, State>,
    id: usize,
) -> Result<TorrentDetailsResponse, ApiError> {
    state.api()?.api_torrent_details(id)
}

#[tauri::command]
async fn torrent_stats(
    state: tauri::State<'_, State>,
    id: usize,
) -> Result<TorrentStats, ApiError> {
    state.api()?.api_stats_v1(id)
}

#[tauri::command]
async fn torrent_action_delete(
    state: tauri::State<'_, State>,
    id: usize,
) -> Result<EmptyJsonResponse, ApiError> {
    state.api()?.api_torrent_action_delete(id)
}

#[tauri::command]
async fn torrent_action_pause(
    state: tauri::State<'_, State>,
    id: usize,
) -> Result<EmptyJsonResponse, ApiError> {
    state.api()?.api_torrent_action_pause(id)
}

#[tauri::command]
async fn torrent_action_forget(
    state: tauri::State<'_, State>,
    id: usize,
) -> Result<EmptyJsonResponse, ApiError> {
    state.api()?.api_torrent_action_forget(id)
}

#[tauri::command]
async fn torrent_action_start(
    state: tauri::State<'_, State>,
    id: usize,
) -> Result<EmptyJsonResponse, ApiError> {
    state.api()?.api_torrent_action_start(id)
}

#[tauri::command]
async fn torrent_action_configure(
    state: tauri::State<'_, State>,
    id: usize,
    only_files: Vec<usize>,
) -> Result<EmptyJsonResponse, ApiError> {
    state
        .api()?
        .api_torrent_action_update_only_files(id, &only_files.into_iter().collect())
}

#[tauri::command]
fn get_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

async fn start() {
    tauri::async_runtime::set(tokio::runtime::Handle::current());
    let init_logging_result = init_logging(InitLoggingOptions {
        default_rust_log_value: Some("info"),
        log_file: None,
        log_file_rust_log: None,
    })
    .unwrap();

    match librqbit::try_increase_nofile_limit() {
        Ok(limit) => info!(limit = limit, "inreased open file limit"),
        Err(e) => warn!("failed increasing open file limit: {:#}", e),
    };

    let state = State::new(init_logging_result).await;

    tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            torrents_list,
            torrent_details,
            torrent_stats,
            torrent_create_from_url,
            torrent_action_delete,
            torrent_action_pause,
            torrent_action_forget,
            torrent_action_start,
            torrent_action_configure,
            torrent_create_from_base64_file,
            get_version,
            config_default,
            config_current,
            config_change,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn main() {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("couldn't set up tokio runtime")
        .block_on(start())
}
