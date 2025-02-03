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
        ApiAddTorrentResponse, EmptyJsonResponse, TorrentDetailsResponse, TorrentIdOrHash,
        TorrentListResponse, TorrentStats,
    },
    dht::PersistentDhtConfig,
    session_stats::snapshot::SessionStatsSnapshot,
    tracing_subscriber_config_utils::{init_logging, InitLoggingOptions, InitLoggingResult},
    AddTorrent, AddTorrentOptions, Api, ApiError, Session, SessionOptions,
    SessionPersistenceConfig,
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
    let mut config: RqbitDesktopConfig = serde_json::from_reader(rdr)?;
    config.persistence.fix_backwards_compat();
    Ok(config)
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
    config
        .validate()
        .context("error validating configuration")?;
    let persistence = if config.persistence.disable {
        None
    } else {
        Some(SessionPersistenceConfig::Json {
            folder: if config.persistence.folder == Path::new("") {
                None
            } else {
                Some(config.persistence.folder.clone())
            },
        })
    };

    let (listen, connect) = config.connections.as_listener_and_connect_opts();

    let session = Session::new_with_opts(
        config.default_download_location.clone(),
        SessionOptions {
            disable_dht: config.dht.disable,
            disable_dht_persistence: config.dht.disable_persistence,
            dht_config: Some(PersistentDhtConfig {
                config_filename: Some(config.dht.persistence_filename.clone()),
                ..Default::default()
            }),
            persistence,
            connect: Some(connect),
            listen,
            fastresume: config.persistence.fastresume,
            ratelimits: config.ratelimits,
            #[cfg(feature = "disable-upload")]
            disable_upload: config.disable_upload,
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
        let listen_addr = config.http_api.listen_addr;
        let api = api.clone();
        let read_only = config.http_api.read_only;
        let upnp_router = if config.upnp.enable_server {
            let friendly_name = config
                .upnp
                .server_friendly_name
                .as_ref()
                .map(|f| f.trim())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_owned())
                .unwrap_or_else(|| {
                    format!(
                        "rqbit-desktop@{}",
                        gethostname::gethostname().to_string_lossy()
                    )
                });

            let mut upnp_adapter = session
                .make_upnp_adapter(friendly_name, config.http_api.listen_addr.port())
                .await
                .context("error starting UPnP server")?;
            let router = upnp_adapter.take_router()?;
            session.spawn(error_span!("ssdp"), async move {
                upnp_adapter.run_ssdp_forever().await
            });
            Some(router)
        } else {
            None
        };
        let http_api_task = async move {
            let listener = tokio::net::TcpListener::bind(listen_addr)
                .await
                .with_context(|| format!("error listening on {}", listen_addr))?;
            librqbit::http_api::HttpApi::new(
                api.clone(),
                Some(librqbit::http_api::HttpApiOptions {
                    read_only,
                    basic_auth: None,
                }),
            )
            .make_http_api_and_run(listener, upnp_router)
            .await
        };

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
            let api = api_from_config(&init_logging, &config)
                .await
                .map_err(|e| {
                    warn!(error=?e, "error reading configuration");
                    e
                })
                .ok();
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
    id: TorrentIdOrHash,
) -> Result<TorrentDetailsResponse, ApiError> {
    state.api()?.api_torrent_details(id)
}

#[tauri::command]
async fn torrent_stats(
    state: tauri::State<'_, State>,
    id: TorrentIdOrHash,
) -> Result<TorrentStats, ApiError> {
    state.api()?.api_stats_v1(id)
}

#[tauri::command]
async fn torrent_action_delete(
    state: tauri::State<'_, State>,
    id: TorrentIdOrHash,
) -> Result<EmptyJsonResponse, ApiError> {
    state.api()?.api_torrent_action_delete(id).await
}

#[tauri::command]
async fn torrent_action_pause(
    state: tauri::State<'_, State>,
    id: TorrentIdOrHash,
) -> Result<EmptyJsonResponse, ApiError> {
    state.api()?.api_torrent_action_pause(id).await
}

#[tauri::command]
async fn torrent_action_forget(
    state: tauri::State<'_, State>,
    id: TorrentIdOrHash,
) -> Result<EmptyJsonResponse, ApiError> {
    state.api()?.api_torrent_action_forget(id).await
}

#[tauri::command]
async fn torrent_action_start(
    state: tauri::State<'_, State>,
    id: TorrentIdOrHash,
) -> Result<EmptyJsonResponse, ApiError> {
    state.api()?.api_torrent_action_start(id).await
}

#[tauri::command]
async fn torrent_action_configure(
    state: tauri::State<'_, State>,
    id: TorrentIdOrHash,
    only_files: Vec<usize>,
) -> Result<EmptyJsonResponse, ApiError> {
    state
        .api()?
        .api_torrent_action_update_only_files(id, &only_files.into_iter().collect())
        .await
}

#[tauri::command]
async fn stats(state: tauri::State<'_, State>) -> Result<SessionStatsSnapshot, ApiError> {
    Ok(state.api()?.api_session_stats())
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
        Ok(limit) => info!(limit = limit, "increased open file limit"),
        Err(e) => warn!("failed increasing open file limit: {:#}", e),
    };

    let state = State::new(init_logging_result).await;

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
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
            stats,
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
