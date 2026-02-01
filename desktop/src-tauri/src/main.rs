// Prevents additional console window on Windows in release, DO NOT REMOVE!!
// Force rebuild
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
use tauri::Emitter;
use http::StatusCode;
use librqbit::{
    AddTorrent, AddTorrentOptions, Api, ApiError, Session, SessionOptions,
    SessionPersistenceConfig, WithStatusError,
    api::{
        ApiAddTorrentResponse, ApiTorrentListOpts, EmptyJsonResponse, TorrentDetailsResponse,
        TorrentIdOrHash, TorrentListResponse, TorrentStats,
    },
    dht::PersistentDhtConfig,
    http_api_types::{PeerStatsFilter, PeerStatsSnapshot},
    session_stats::snapshot::SessionStatsSnapshot,
    tracing_subscriber_config_utils::{InitLoggingOptions, InitLoggingResult, init_logging},
    CreateTorrentOptions,
};
use librqbit_dualstack_sockets::TcpListener;
use parking_lot::RwLock;
use serde::Serialize;
use tracing::{debug_span, error, info, warn};

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

    let mut http_api_opts = librqbit::http_api::HttpApiOptions {
        read_only: config.http_api.read_only,
        basic_auth: if let Some(up) = &config.http_api.basic_auth {
            let (u, p) = up
                .split_once(":")
                .context("basic auth credentials should be in format username:password")?;
            Some((u.to_owned(), p.to_owned()))
        } else {
            None
        },
        ..Default::default()
    };

    // We need to start prometheus recorder earlier than session.
    if !config.http_api.disable {
        match metrics_exporter_prometheus::PrometheusBuilder::new().install_recorder() {
            Ok(handle) => {
                http_api_opts.prometheus_handle = Some(handle);
            }
            Err(e) => {
                warn!("error installting prometheus recorder: {e:#}");
            }
        }
    }

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
            kill_locking_processes: config.features.kill_locking_processes,
            sync_extra_files: config.features.sync_extra_files,
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
            session.spawn(debug_span!("ssdp"), "ssdp", async move {
                upnp_adapter.run_ssdp_forever().await
            });
            Some(router)
        } else {
            None
        };
        let http_api_task = async move {
            let listener = TcpListener::bind_tcp(listen_addr, Default::default())
                .with_context(|| format!("error listening on {}", listen_addr))?;
            librqbit::http_api::HttpApi::new(api.clone(), Some(http_api_opts))
                .make_http_api_and_run(listener, upnp_router)
                .await
        };

        session.spawn(debug_span!("http_api"), "http_api", http_api_task);
    }
    Ok(api)
}

impl State {
    async fn new(init_logging: InitLoggingResult) -> Self {
        let mut config_filename = directories::ProjectDirs::from("com", "rqbit", "desktop")
            .expect("directories::ProjectDirs::from")
            .config_dir()
            .join("config.json");

        // Portable mode: check if config.json exists in the same directory as the executable
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                let portable_config = exe_dir.join("config.json");
                if portable_config.exists() {
                    info!("Using portable config: {:?}", portable_config);
                    config_filename = portable_config;
                }
            }
        }

        let config_filename = config_filename.to_str().expect("to_str()").to_owned();

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
        g.as_ref()
            .and_then(|a| a.api.clone())
            .with_status_error(StatusCode::FAILED_DEPENDENCY, "not configured")
    }

    async fn configure(&self, config: RqbitDesktopConfig) -> Result<(), ApiError> {
        {
            let g = self.shared.read();
            if let Some(shared) = g.as_ref()
                && shared.api.is_some()
                && shared.config == config
            {
                // The config didn't change, and the API is running, nothing to do.
                return Ok(());
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
    Ok(state
        .api()?
        .api_torrent_list_ext(ApiTorrentListOpts { with_stats: true }))
}

#[tauri::command]
fn torrent_haves(
    state: tauri::State<State>,
    id: TorrentIdOrHash,
) -> Result<tauri::ipc::InvokeResponseBody, ApiError> {
    let (haves, _len) = state.api()?.api_dump_haves(id)?;
    Ok(tauri::ipc::InvokeResponseBody::Raw(
        haves.into_boxed_slice().into(),
    ))
}

#[tauri::command]
fn torrent_peer_stats(
    state: tauri::State<State>,
    id: TorrentIdOrHash,
    filter: PeerStatsFilter,
) -> Result<PeerStatsSnapshot, ApiError> {
    state.api()?.api_peer_stats(id, filter)
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
    use base64::{Engine as _, engine::general_purpose};
    let bytes = general_purpose::STANDARD
        .decode(&contents)
        .with_status_error(StatusCode::BAD_REQUEST, "invalid base64")?;
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
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![]),
        ))
        .plugin(tauri_plugin_dialog::init())
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                window.hide().unwrap();
                api.prevent_close();
            }
        })
        .setup(|app| {


             use tauri::menu::{Menu, MenuItem, CheckMenuItem};
             use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
             use tauri::Manager;
             use tauri_plugin_autostart::ManagerExt;

             let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>).ok();
             let show_i = MenuItem::with_id(app, "show", "Show", true, None::<&str>).ok();
             let autostart_i = CheckMenuItem::with_id(app, "autostart", "Start at Login", true, false, None::<&str>).ok();
             
             if let (Some(quit_i), Some(show_i), Some(autostart_i)) = (quit_i, show_i, autostart_i) {
                 // Check current autostart status
                 if let Ok(true) = app.autolaunch().is_enabled() {
                     let _ = autostart_i.set_checked(true);
                 }

                  let menu = Menu::with_items(app, &[&show_i, &autostart_i, &quit_i]);
                  if let Ok(menu) = menu {
                     let _ = TrayIconBuilder::new()
                        .icon(app.default_window_icon().unwrap().clone())
                        .tooltip("rqbit Headless")
                        .menu(&menu)
                        .on_menu_event(move |app, event| {
                            match event.id().as_ref() {
                                "quit" => {
                                    app.exit(0);
                                }
                                "show" => {
                                    if let Some(window) = app.get_webview_window("main") {
                                        let _ = window.show();
                                        let _ = window.set_focus();
                                    }
                                }
                                "autostart" => {
                                    let enabled = app.autolaunch().is_enabled().unwrap_or(false);
                                    if enabled {
                                        let _ = app.autolaunch().disable();
                                        let _ = autostart_i.set_checked(false);
                                    } else {
                                        let _ = app.autolaunch().enable();
                                        let _ = autostart_i.set_checked(true);
                                    }
                                }
                                _ => {}
                            }
                        })
                        .on_tray_icon_event(|tray, event| {
                            if let TrayIconEvent::Click {
                                button: MouseButton::Left,
                                ..
                            } = event
                            {
                                let app = tray.app_handle();
                                if let Some(window) = app.get_webview_window("main") {
                                    let _ = window.show();
                                    let _ = window.set_focus();
                                }
                            }
                        })
                        .build(app);
                  }
             }
             
             // Force show window on startup
             if let Some(window) = app.get_webview_window("main") {
                 let _ = window.show();
                 let _ = window.set_focus();
             }

             Ok(())
        })
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            config_change,
            config_current,
            config_default,
            get_version,
            stats,
            torrent_action_configure,
            torrent_action_delete,
            torrent_action_forget,
            torrent_action_pause,
            torrent_action_start,
            torrent_create,
            torrent_create_from_base64_file,
            torrent_create_from_url,
            torrent_details,
            torrent_haves,
            torrent_peer_stats,
            torrent_stats,
            torrents_list,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[tauri::command]
async fn torrent_create(
    state: tauri::State<'_, State>,
    window: tauri::Window,
    path: String,
    name: Option<String>,
    trackers: Option<Vec<String>>,
) -> Result<ApiAddTorrentResponse, ApiError> {
    let opts = CreateTorrentOptions {
        name: name,
        trackers: trackers.unwrap_or_default(),
        piece_length: None,
    };
    let path = std::path::PathBuf::from(path);
    let api = state.api()?;
    
    // Accumulate bytes and time for throttling
    use std::sync::{Arc, Mutex};
    use std::time::{Instant, Duration};
    // (last_emit_time, accumulated_bytes)
    let throttle_state = Arc::new(Mutex::new((Instant::now(), 0u64)));
    
    let window_clone = window.clone();
    let throttle_clone = throttle_state.clone();

    let (_meta, handle) = api
        .session()
        .create_and_serve_torrent(&path, opts, move |chunk, total| {
            let mut state = throttle_clone.lock().unwrap();
            state.1 += chunk; // Accumulate bytes

            // Emit if > 100ms passed OR if this is the final final chunk (approximate check, but we can't know for sure if it's the last call unless chunk==0 or similar, but total is constant).
            // Actually, we should just emit if time passed.
            // Also always emit if we have accumulated a significant amount? No, time is better for UI.
            
            if state.0.elapsed() > Duration::from_millis(2000) {
                 let _ = window_clone.emit("create_torrent_progress", serde_json::json!({
                    "chunk": state.1,
                    "total": total
                }));
                state.0 = Instant::now();
                state.1 = 0;
            }
        })
        .await
        .map_err(ApiError::from)?;
        
    // Emit any remaining bytes
    let last_chunk = {
         let state = throttle_state.lock().unwrap();
         state.1
    };
    if last_chunk > 0 {
         let _ = window.emit("create_torrent_progress", serde_json::json!({
            "chunk": last_chunk,
             // best guess for total, though frontend updates total from payload anyway
            "total": 0 
        }));
    }

    let details = api.api_torrent_details(librqbit::api::TorrentIdOrHash::Id(handle.id()))?;
    
    Ok(ApiAddTorrentResponse {
        id: Some(handle.id()),
        output_folder: details.output_folder.clone(),
        details,
        seen_peers: None,
    })
}

fn main() {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("couldn't set up tokio runtime")
        .block_on(start())
}
// force rebuild
