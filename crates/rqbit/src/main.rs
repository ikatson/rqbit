use std::{
    collections::HashSet,
    io,
    net::{IpAddr, SocketAddr},
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::Duration,
};

use anyhow::{bail, Context};
use clap::{CommandFactory, Parser, ValueEnum};
use clap_complete::Shell;
use librqbit::{
    api::ApiAddTorrentResponse,
    http_api::{HttpApi, HttpApiOptions},
    http_api_client, librqbit_spawn,
    limits::LimitsConfig,
    storage::{
        filesystem::{FilesystemStorageFactory, MmapFilesystemStorageFactory},
        StorageFactory, StorageFactoryExt,
    },
    tracing_subscriber_config_utils::{init_logging, InitLoggingOptions},
    AddTorrent, AddTorrentOptions, AddTorrentResponse, Api, ConnectionOptions, ListOnlyResponse,
    ListenerMode, ListenerOptions, PeerConnectionOptions, Session, SessionOptions,
    SessionPersistenceConfig, TorrentStatsState,
};
use size_format::SizeFormatterBinary as SF;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{error, error_span, info, trace_span, warn};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[cfg(not(target_os = "windows"))]
fn parse_umask(value: &str) -> anyhow::Result<libc::mode_t> {
    fn parse_oct_digit(d: u8) -> Option<libc::mode_t> {
        Some(match d {
            b'0' => 0,
            b'1' => 1,
            b'2' => 2,
            b'3' => 3,
            b'4' => 4,
            b'5' => 5,
            b'6' => 6,
            b'7' => 7,
            _ => return None,
        })
    }
    if value.len() != 3 {
        bail!("expected 3 digits")
    }
    let mut output = 0;
    for digit in value.as_bytes() {
        let digit = parse_oct_digit(*digit).context("expected 3 digits")?;
        output = output * 8 + digit;
    }
    Ok(output)
}

#[derive(Parser)]
#[command(version, author, about)]
struct Opts {
    /// The console loglevel
    #[arg(value_enum, short = 'v', env = "RQBIT_LOG_LEVEL_CONSOLE")]
    log_level: Option<LogLevel>,

    /// The log filename to also write to in addition to the console.
    #[arg(long = "log-file", env = "RQBIT_LOG_FILE")]
    log_file: Option<String>,

    /// The value for RUST_LOG in the log file
    #[arg(
        long = "log-file-rust-log",
        default_value = "librqbit=debug,info",
        env = "RQBIT_LOG_FILE_RUST_LOG"
    )]
    log_file_rust_log: String,

    /// The interval to poll trackers, e.g. 30s.
    /// Trackers send the refresh interval when we connect to them. Often this is
    /// pretty big, e.g. 30 minutes. This can force a certain value.
    #[arg(short = 'i', long = "tracker-refresh-interval", value_parser = parse_duration::parse, env="RQBIT_TRACKER_REFRESH_INTERVAL")]
    force_tracker_interval: Option<Duration>,

    /// The listen address for HTTP API
    #[arg(
        long = "http-api-listen-addr",
        default_value = "127.0.0.1:3030",
        env = "RQBIT_HTTP_API_LISTEN_ADDR"
    )]
    http_api_listen_addr: SocketAddr,

    /// Set this flag if you want to use tokio's single threaded runtime.
    /// It MAY perform better, but the main purpose is easier debugging, as time
    /// profilers work better with this one.
    #[arg(short, long, env = "RQBIT_SINGLE_THREAD_RUNTIME")]
    single_thread_runtime: bool,

    #[arg(long = "disable-dht", env = "RQBIT_DHT_DISABLE")]
    disable_dht: bool,

    /// Set this to disable DHT reading and storing it's state.
    /// For now this is a useful workaround if you want to launch multiple rqbit instances,
    /// otherwise DHT port will conflict.
    #[arg(
        long = "disable-dht-persistence",
        env = "RQBIT_DHT_PERSISTENCE_DISABLE"
    )]
    disable_dht_persistence: bool,

    /// The connect timeout, e.g. 1s, 1.5s, 100ms etc.
    #[arg(long = "peer-connect-timeout", value_parser = parse_duration::parse, default_value="2s", env="RQBIT_PEER_CONNECT_TIMEOUT")]
    peer_connect_timeout: Duration,

    /// The connect timeout, e.g. 1s, 1.5s, 100ms etc.
    #[arg(long = "peer-read-write-timeout" , value_parser = parse_duration::parse, default_value="10s", env="RQBIT_PEER_READ_WRITE_TIMEOUT")]
    peer_read_write_timeout: Duration,

    /// How many threads to spawn for the executor.
    #[arg(short = 't', long, env = "RQBIT_RUNTIME_WORKER_THREADS")]
    worker_threads: Option<usize>,

    // Enable to listen on 0.0.0.0 on TCP for torrent requests.
    #[arg(long = "disable-tcp-listen", env = "RQBIT_TCP_LISTEN_DISABLE")]
    disable_tcp_listen: bool,

    // Disable connecting over TCP. Only uTP will be used (if enabled).
    #[arg(long = "disable-tcp-connect", env = "RQBIT_TCP_CONNECT_DISABLE")]
    disable_tcp_connect: bool,

    // Enable to listen and connect over uTP
    #[arg(
        long = "experimental-enable-utp-listen",
        env = "RQBIT_EXPERIMENTAL_UTP_LISTEN_ENABLE"
    )]
    enable_utp_listen: bool,

    /// The port to listen for incoming connections (applies to both TCP and uTP).
    #[arg(
        long = "listen-port",
        default_value = "4240",
        env = "RQBIT_LISTEN_PORT"
    )]
    listen_port: u16,

    /// What's the IP to listen on. Default is to listen on all interfaces.
    #[arg(long = "listen-ip", default_value = "0.0.0.0", env = "RQBIT_LISTEN_IP")]
    listen_ip: IpAddr,

    /// If set, will try to publish the chosen port through upnp on your router.
    /// If the listen-ip is localhost, this will not be used.
    #[arg(
        long = "disable-upnp-port-forward",
        env = "RQBIT_UPNP_PORT_FORWARD_DISABLE"
    )]
    disable_upnp_port_forward: bool,

    /// If set, will run a UPNP Media server on RQBIT_HTTP_API_LISTEN_ADDR.
    #[arg(long = "enable-upnp-server", env = "RQBIT_UPNP_SERVER_ENABLE")]
    enable_upnp_server: bool,

    /// UPNP server name that would be displayed on devices in your network.
    #[arg(
        long = "upnp-server-friendly-name",
        env = "RQBIT_UPNP_SERVER_FRIENDLY_NAME"
    )]
    upnp_server_friendly_name: Option<String>,

    #[command(subcommand)]
    subcommand: SubCommand,

    /// How many maximum blocking tokio threads to spawn to process disk reads/writes.
    /// This will indicate how many parallel reads/writes can happen at a moment in time.
    /// The higher the number, the more the memory usage.
    #[arg(
        long = "max-blocking-threads",
        default_value = "8",
        env = "RQBIT_RUNTIME_MAX_BLOCKING_THREADS"
    )]
    max_blocking_threads: u16,

    // If you set this to something, all writes to disk will happen in background and be
    // buffered in memory up to approximately the given number of megabytes.
    //
    // Might be useful for slow disks.
    #[arg(long = "defer-writes-up-to", env = "RQBIT_DEFER_WRITES_UP_TO")]
    defer_writes_up_to: Option<usize>,

    /// Use mmap (file-backed) for storage. Any advantages are questionable and unproven.
    /// If you use it, you know what you are doing.
    #[arg(long)]
    experimental_mmap_storage: bool,

    /// Provide a socks5 URL.
    /// The format is socks5://[username:password]@host:port
    ///
    /// Alternatively, set this as an environment variable RQBIT_SOCKS_PROXY_URL
    #[arg(long, env = "RQBIT_SOCKS_PROXY_URL")]
    socks_url: Option<String>,

    /// How many torrents can be initializing (rehashing) at the same time
    #[arg(long, default_value = "5", env = "RQBIT_CONCURRENT_INIT_LIMIT")]
    concurrent_init_limit: usize,

    /// Set the process umask to this value.
    ///
    /// Default is inherited from your environment (usually 022).
    /// This will affect the file mode of created files.
    ///
    /// Read more at https://man7.org/linux/man-pages/man2/umask.2.html
    #[cfg(not(target_os = "windows"))]
    #[arg(long, env = "RQBIT_UMASK", value_parser=parse_umask)]
    umask: Option<libc::mode_t>,

    /// Disable uploading entirely. If this is set, rqbit won't share piece availability
    /// and will disconnect on download request.
    ///
    /// Might be useful e.g. if rqbit upload consumes all your upload bandwidth and interferes
    /// with your other Internet usage.
    #[cfg(feature = "disable-upload")]
    #[arg(long, env = "RQBIT_DISABLE_UPLOAD")]
    disable_upload: bool,

    /// Limit download to bytes-per-second.
    #[arg(long = "ratelimit-download", env = "RQBIT_RATELIMIT_DOWNLOAD")]
    ratelimit_download_bps: Option<NonZeroU32>,

    /// Limit upload to bytes-per-second.
    #[arg(long = "ratelimit-upload", env = "RQBIT_RATELIMIT_UPLOAD")]
    ratelimit_upload_bps: Option<NonZeroU32>,

    /// Downloads a p2p blocklist from this url and blocks peers from it
    #[arg(long, env = "RQBIT_BLOCKLIST_URL")]
    blocklist_url: Option<String>,

    /// The filename with tracker URLs to always use for each torrent.
    #[arg(long, env = "RQBIT_TRACKERS_FILENAME")]
    trackers_filename: Option<String>,
}

#[derive(Parser)]
struct ServerStartOptions {
    /// The output folder to write to. If not exists, it will be created.
    output_folder: String,

    #[arg(
        long = "disable-persistence",
        help = "Disable server persistence. It will not read or write its state to disk.",
        env = "RQBIT_SESSION_PERSISTENCE_DISABLE"
    )]
    /// Disable session persistence.
    disable_persistence: bool,

    /// The folder to store session data in. By default uses OS specific folder.
    /// If starts with postgres://, will use postgres as the backend instead of JSON file.
    #[arg(
        long = "persistence-location",
        env = "RQBIT_SESSION_PERSISTENCE_LOCATION"
    )]
    persistence_location: Option<String>,

    /// [Experimental] if set, will try to resume quickly after restart and skip checksumming.
    #[arg(long = "fastresume", env = "RQBIT_FASTRESUME")]
    fastresume: bool,

    /// Enable prometheus exporter endpoint at HTTP_API_LISTEN_ADDR:3030/metrics
    #[cfg(feature = "prometheus")]
    #[arg(
        long = "enable-prometheus-exporter",
        env = "RQBIT_ENABLE_PROMETHEUS_EXPORTER"
    )]
    enable_prometheus_exporter: bool,

    /// The folder to watch for added .torrent files. All files in this folder will be automatically added
    /// to the session.
    #[arg(long = "watch-folder", env = "RQBIT_WATCH_FOLDER")]
    watch_folder: Option<String>,
}

#[derive(Parser)]
struct ServerOpts {
    #[clap(subcommand)]
    subcommand: ServerSubcommand,
}

#[derive(Parser)]
enum ServerSubcommand {
    Start(ServerStartOptions),
}

#[derive(Parser)]
struct DownloadOpts {
    /// The filename or URL of the torrent. If URL, http/https/magnet are supported.
    torrent_path: Vec<String>,

    /// The output folder to write to. If not exists, it will be created.
    /// If not specified, would use the server's output folder. If there's no server
    /// running, this is required.
    #[arg(short = 'o', long)]
    output_folder: Option<String>,

    /// The sub folder within output folder to write to. Useful when you have
    /// a server running with output_folder configured, and don't want to specify
    /// the full path every time.
    #[arg(short = 's', long)]
    sub_folder: Option<String>,

    /// If set, only the file whose filename matching this regex will
    /// be downloaded
    #[arg(short = 'r', long = "filename-re")]
    only_files_matching_regex: Option<String>,

    /// Only list the torrent metadata contents, don't do anything else.
    #[arg(short, long)]
    list: bool,

    /// Set if you are ok to write on top of existing files
    #[arg(long)]
    overwrite: bool,

    /// Exit the program once the torrents complete download.
    #[arg(short = 'e', long)]
    exit_on_finish: bool,

    #[arg(long = "disable-trackers")]
    disable_trackers: bool,

    #[arg(long = "initial-peers")]
    initial_peers: Option<InitialPeers>,

    #[arg(long = "server-url")]
    server_url: Option<String>,
}

#[derive(Clone)]
struct InitialPeers(Vec<SocketAddr>);

impl From<&str> for InitialPeers {
    fn from(s: &str) -> Self {
        let mut v = Vec::new();
        for addr in s.split(',') {
            v.push(addr.parse().unwrap());
        }
        Self(v)
    }
}

#[derive(Parser)]
struct CompletionsOpts {
    /// The shell to generate completions for
    shell: Shell,
}

// server start
// download [--connect-to-existing] --output-folder(required) [file1] [file2]

#[derive(Parser)]
enum SubCommand {
    Server(ServerOpts),
    Download(DownloadOpts),
    Completions(CompletionsOpts),
}

fn _start_deadlock_detector_thread() {
    use parking_lot::deadlock;
    use std::thread;

    // Create a background thread which checks for deadlocks every 10s
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(10));
        let deadlocks = deadlock::check_deadlock();
        if deadlocks.is_empty() {
            continue;
        }

        println!("{} deadlocks detected", deadlocks.len());
        for (i, threads) in deadlocks.iter().enumerate() {
            println!("Deadlock #{}", i);
            for t in threads {
                println!("Thread Id {:#?}", t.thread_id());
                println!("{:#?}", t.backtrace());
            }
        }
        std::process::exit(42);
    });
}

fn main() -> anyhow::Result<()> {
    let opts = Opts::parse();

    #[cfg(not(target_os = "windows"))]
    if let Some(umask) = opts.umask {
        unsafe { libc::umask(umask) };
    }

    let mut rt_builder = match opts.single_thread_runtime {
        true => tokio::runtime::Builder::new_current_thread(),
        false => {
            let mut b = tokio::runtime::Builder::new_multi_thread();
            if let Some(e) = opts.worker_threads {
                b.worker_threads(e);
            }
            b
        }
    };

    let rt = rt_builder
        .enable_time()
        .enable_io()
        // the default is 512, it can get out of hand, as this program is CPU-bound on
        // hash checking.
        // note: we aren't using spawn_blocking() anymore, so this doesn't apply,
        // however I'm still messing around, so in case we do, let's block the number of
        // spawned threads.
        .max_blocking_threads(opts.max_blocking_threads as usize)
        .build()?;

    let token = tokio_util::sync::CancellationToken::new();
    #[cfg(not(target_os = "windows"))]
    {
        let token = token.clone();
        use signal_hook::{consts::SIGINT, consts::SIGTERM, iterator::Signals};
        let mut signals = Signals::new([SIGINT, SIGTERM])?;
        thread::spawn(move || {
            if let Some(sig) = signals.forever().next() {
                warn!("received signal {:?}, shutting down", sig);
                token.cancel();
                std::thread::sleep(Duration::from_secs(5));
                warn!("could not shutdown in time, killing myself");
                std::process::exit(1)
            }
        });
    }

    let result = rt.block_on(async_main(opts, token.clone()));
    if let Err(e) = result.as_ref() {
        error!("error running rqbit: {e:?}");
    }
    rt.shutdown_timeout(Duration::from_secs(1));
    match result {
        Ok(_) => std::process::exit(0),
        Err(_) => std::process::exit(1),
    }
}

async fn parse_trackers_file(filename: &str) -> anyhow::Result<HashSet<url::Url>> {
    let content = tokio::fs::read_to_string(filename)
        .await
        .with_context(|| format!("error opening {filename}"))?;
    let trackers = content
        .lines()
        .filter_map(|s| {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            url::Url::parse(s).ok()
        })
        .collect::<HashSet<url::Url>>();
    info!(filename, count = trackers.len(), "parsed trackers");
    Ok(trackers)
}

async fn async_main(opts: Opts, cancel: CancellationToken) -> anyhow::Result<()> {
    let log_config = init_logging(InitLoggingOptions {
        default_rust_log_value: Some(match opts.log_level.unwrap_or(LogLevel::Info) {
            LogLevel::Trace => "trace",
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }),
        log_file: opts.log_file.as_deref(),
        log_file_rust_log: Some(&opts.log_file_rust_log),
    })?;

    match librqbit::try_increase_nofile_limit() {
        Ok(limit) => info!(limit = limit, "increased open file limit"),
        Err(e) => warn!("failed increasing open file limit: {:#}", e),
    };

    let trackers = if let Some(f) = opts.trackers_filename {
        parse_trackers_file(&f)
            .await
            .context("error reading trackers file")?
    } else {
        Default::default()
    };

    let listen_mode = match (!opts.disable_tcp_listen, opts.enable_utp_listen) {
        (true, false) => Some(ListenerMode::TcpOnly),
        (false, true) => Some(ListenerMode::UtpOnly),
        (true, true) => Some(ListenerMode::TcpAndUtp),
        (false, false) => None,
    };
    let listen = listen_mode.map(|mode| ListenerOptions {
        mode,
        listen_addr: (opts.listen_ip, opts.listen_port).into(),
        enable_upnp_port_forwarding: !opts.disable_upnp_port_forward,
        ..Default::default()
    });

    let mut sopts = SessionOptions {
        disable_dht: opts.disable_dht,
        disable_dht_persistence: opts.disable_dht_persistence,
        dht_config: None,
        // This will be overriden by "server start" below if needed.
        persistence: None,
        peer_id: None,
        listen,
        connect: Some(ConnectionOptions {
            proxy_url: opts.socks_url,
            enable_tcp: !opts.disable_tcp_connect,
            peer_opts: Some(PeerConnectionOptions {
                connect_timeout: Some(opts.peer_connect_timeout),
                read_write_timeout: Some(opts.peer_read_write_timeout),
                ..Default::default()
            }),
        }),
        defer_writes_up_to: opts.defer_writes_up_to,
        default_storage_factory: Some({
            fn wrap<S: StorageFactory + Clone>(s: S) -> impl StorageFactory {
                #[cfg(feature = "debug_slow_disk")]
                {
                    use librqbit::storage::middleware::{
                        slow::SlowStorageFactory, timing::TimingStorageFactory,
                    };
                    TimingStorageFactory::new("hdd".to_owned(), SlowStorageFactory::new(s))
                }
                #[cfg(not(feature = "debug_slow_disk"))]
                s
            }

            if opts.experimental_mmap_storage {
                wrap(MmapFilesystemStorageFactory::default()).boxed()
            } else {
                wrap(FilesystemStorageFactory::default()).boxed()
            }
        }),
        concurrent_init_limit: Some(opts.concurrent_init_limit),
        root_span: None,
        fastresume: false,
        cancellation_token: Some(cancel.clone()),
        #[cfg(feature = "disable-upload")]
        disable_upload: opts.disable_upload,
        ratelimits: LimitsConfig {
            upload_bps: opts.ratelimit_upload_bps,
            download_bps: opts.ratelimit_download_bps,
        },
        blocklist_url: opts.blocklist_url,
        trackers,
    };

    let http_api_basic_auth = if let Ok(up) = std::env::var("RQBIT_HTTP_BASIC_AUTH_USERPASS") {
        let (u, p) = up
            .split_once(":")
            .context("basic auth credentials should be in format username:password")?;
        Some((u.to_owned(), p.to_owned()))
    } else {
        None
    };

    let stats_printer = |session: Arc<Session>| async move {
        loop {
            session.with_torrents(|torrents| {
                    for (idx, torrent) in torrents {
                        let stats = torrent.stats();
                        if let TorrentStatsState::Initializing = stats.state {
                            let total = stats.total_bytes;
                            let progress = stats.progress_bytes;
                            let pct =  (progress as f64 / total as f64) * 100f64;
                            info!("[{}] initializing {:.2}%", idx, pct);
                            continue;
                        }
                        let (live, live_stats) = match (torrent.live(), stats.live.as_ref()) {
                            (Some(live), Some(live_stats)) => (live, live_stats),
                            _ => continue
                        };
                        let down_speed = live.down_speed_estimator();
                        let up_speed = live.up_speed_estimator();
                        let total = stats.total_bytes;
                        let progress = stats.progress_bytes;
                        let downloaded_pct = if stats.finished {
                            100f64
                        } else {
                            (progress as f64 / total as f64) * 100f64
                        };
                        let time_remaining = down_speed.time_remaining();
                        let eta = match &time_remaining {
                            Some(d) => format!(", ETA: {:?}", d),
                            None => String::new()
                        };
                        let peer_stats = &live_stats.snapshot.peer_stats;
                        info!(
                            "[{}]: {:.2}% ({:.2} / {:.2}), ↓{:.2} MiB/s, ↑{:.2} MiB/s ({:.2}){}, {{live: {}, queued: {}, dead: {}, known: {}}}",
                            idx,
                            downloaded_pct,
                            SF::new(progress),
                            SF::new(total),
                            down_speed.mbps(),
                            up_speed.mbps(),
                            SF::new(live_stats.snapshot.uploaded_bytes),
                            eta,
                            peer_stats.live,
                            peer_stats.queued + peer_stats.connecting,
                            peer_stats.dead,
                            peer_stats.seen,
                        );
                    }
                });
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    };

    match &opts.subcommand {
        SubCommand::Server(server_opts) => match &server_opts.subcommand {
            ServerSubcommand::Start(start_opts) => {
                if !start_opts.disable_persistence {
                    if let Some(p) = start_opts.persistence_location.as_ref() {
                        if p.starts_with("postgres://") {
                            #[cfg(feature = "postgres")]
                            {
                                sopts.persistence = Some(SessionPersistenceConfig::Postgres {
                                    connection_string: p.clone(),
                                })
                            }
                            #[cfg(not(feature = "postgres"))]
                            {
                                anyhow::bail!("rqbit was compiled without postgres support")
                            }
                        } else {
                            sopts.persistence = Some(SessionPersistenceConfig::Json {
                                folder: Some(p.into()),
                            })
                        }
                    } else {
                        sopts.persistence = Some(SessionPersistenceConfig::Json { folder: None })
                    }
                }

                let mut http_api_opts = HttpApiOptions {
                    read_only: false,
                    basic_auth: http_api_basic_auth,
                    ..Default::default()
                };

                // We need to install prometheus recorder early before we registered any metrics.
                if cfg!(feature = "prometheus") && start_opts.enable_prometheus_exporter {
                    match metrics_exporter_prometheus::PrometheusBuilder::new().install_recorder() {
                        Ok(handle) => {
                            http_api_opts.prometheus_handle = Some(handle);
                        }
                        Err(e) => {
                            warn!("error installting prometheus recorder: {e:#}");
                        }
                    }
                }

                sopts.fastresume = start_opts.fastresume;

                let session =
                    Session::new_with_opts(PathBuf::from(&start_opts.output_folder), sopts)
                        .await
                        .context("error initializing rqbit session")?;
                librqbit_spawn(
                    "stats_printer",
                    trace_span!("stats_printer"),
                    stats_printer(session.clone()),
                );

                let mut upnp_server = {
                    match opts.enable_upnp_server {
                        true => {
                            if opts.http_api_listen_addr.ip().is_loopback() {
                                bail!("cannot enable UPNP server as HTTP API listen addr is localhost. Change --http-api-listen-addr to start with 0.0.0.0");
                            }
                            let server = session
                                .make_upnp_adapter(
                                    opts.upnp_server_friendly_name.unwrap_or_else(|| {
                                        format!(
                                            "rqbit@{}",
                                            gethostname::gethostname().to_string_lossy()
                                        )
                                    }),
                                    opts.http_api_listen_addr.port(),
                                )
                                .await
                                .context("error starting UPNP server")?;
                            Some(server)
                        }
                        false => None,
                    }
                };

                let api = Api::new(
                    session.clone(),
                    Some(log_config.rust_log_reload_tx),
                    Some(log_config.line_broadcast),
                );
                let http_api = HttpApi::new(api, Some(http_api_opts));
                let http_api_listen_addr = opts.http_api_listen_addr;

                info!("starting HTTP API at http://{http_api_listen_addr}");
                let tcp_listener = TcpListener::bind(http_api_listen_addr)
                    .await
                    .with_context(|| format!("error binding to {http_api_listen_addr}"))?;

                let upnp_router = upnp_server.as_mut().and_then(|s| s.take_router().ok());
                let http_api_fut = http_api.make_http_api_and_run(tcp_listener, upnp_router);

                if let Some(watch_folder) = start_opts.watch_folder.as_ref() {
                    session.watch_folder(Path::new(watch_folder));
                }

                let res = match upnp_server {
                    Some(srv) => {
                        let upnp_fut = srv.run_ssdp_forever();

                        tokio::select! {
                            r = http_api_fut => r,
                            r = upnp_fut => r
                        }
                    }
                    None => tokio::select! {
                        _ = cancel.cancelled() => bail!("cancelled"),
                        r = http_api_fut => r,
                    },
                };
                res.context("error running server")
            }
        },
        SubCommand::Download(download_opts) => {
            if download_opts.torrent_path.is_empty() {
                anyhow::bail!("you must provide at least one URL to download")
            }
            let http_api_url = download_opts
                .server_url
                .clone()
                .unwrap_or_else(|| format!("http://{}", opts.http_api_listen_addr));
            let client = http_api_client::HttpApiClient::new(&http_api_url)?;

            let torrent_opts = |with_output_folder: bool| AddTorrentOptions {
                only_files_regex: download_opts.only_files_matching_regex.clone(),
                overwrite: download_opts.overwrite,
                list_only: download_opts.list,
                force_tracker_interval: opts.force_tracker_interval,
                output_folder: if with_output_folder {
                    download_opts.output_folder.clone()
                } else {
                    None
                },
                sub_folder: download_opts.sub_folder.clone(),
                initial_peers: download_opts.initial_peers.clone().map(|p| p.0),
                disable_trackers: download_opts.disable_trackers,
                ..Default::default()
            };
            let connect_to_existing = match client.validate_rqbit_server().await {
                Ok(_) => {
                    info!("Connected to HTTP API at {}, will call it instead of downloading within this process", client.base_url());
                    true
                }
                Err(err) => {
                    warn!("Error checking HTTP API at {}: {:}", client.base_url(), err);
                    false
                }
            };
            if !connect_to_existing && download_opts.server_url.is_some() {
                anyhow::bail!("cannot connect to server at {}", client.base_url());
            }
            if connect_to_existing {
                for torrent_url in &download_opts.torrent_path {
                    match client
                        .add_torrent(
                            AddTorrent::from_cli_argument(torrent_url)?,
                            Some(torrent_opts(true)),
                        )
                        .await
                    {
                        Ok(ApiAddTorrentResponse { id, details, .. }) => {
                            if let Some(id) = id {
                                info!("{} added to the server with index {}. Query {}/torrents/{}/(stats/haves) for details", details.info_hash, id, http_api_url, id)
                            }
                            for file in details.files.into_iter().flat_map(|i| i.into_iter()) {
                                info!(
                                    "file {:?}, size {}{}",
                                    file.name,
                                    SF::new(file.length),
                                    if file.included { "" } else { ", will skip" }
                                )
                            }
                        }
                        Err(err) => warn!("error adding {}: {:?}", torrent_url, err),
                    }
                }
                Ok(())
            } else {
                let session = Session::new_with_opts(
                    download_opts
                        .output_folder
                        .as_ref()
                        .map(PathBuf::from)
                        .context(
                            "output_folder is required if can't connect to an existing server",
                        )?,
                    sopts,
                )
                .await
                .context("error initializing rqbit session")?;

                librqbit_spawn(
                    "stats_printer",
                    trace_span!("stats_printer"),
                    stats_printer(session.clone()),
                );
                let api = Api::new(
                    session.clone(),
                    Some(log_config.rust_log_reload_tx),
                    Some(log_config.line_broadcast),
                );
                let http_api = HttpApi::new(
                    api,
                    Some(HttpApiOptions {
                        read_only: true,
                        basic_auth: http_api_basic_auth,
                        ..Default::default()
                    }),
                );
                let http_api_listen_addr = opts.http_api_listen_addr;

                info!("starting HTTP API at http://{http_api_listen_addr}");
                let listener = tokio::net::TcpListener::bind(opts.http_api_listen_addr)
                    .await
                    .with_context(|| format!("error binding to {http_api_listen_addr}"))?;

                librqbit_spawn(
                    "http_api",
                    error_span!("http_api"),
                    http_api.make_http_api_and_run(listener, None),
                );

                let mut added = false;

                let mut handles = Vec::new();

                for path in &download_opts.torrent_path {
                    let handle = match session
                        .add_torrent(
                            AddTorrent::from_cli_argument(path)?,
                            Some(torrent_opts(false)),
                        )
                        .await
                    {
                        Ok(v) => match v {
                            AddTorrentResponse::AlreadyManaged(id, handle) => {
                                info!(
                                    "torrent {:?} is already managed, id={}",
                                    handle.info_hash(),
                                    id,
                                );
                                continue;
                            }
                            AddTorrentResponse::ListOnly(ListOnlyResponse {
                                info_hash: _,
                                info,
                                only_files,
                                ..
                            }) => {
                                for (idx, fd) in info.iter_file_details()?.enumerate() {
                                    let included = match &only_files {
                                        Some(files) => files.contains(&idx),
                                        None => true,
                                    };
                                    info!(
                                        "File {:?}, size {}{}",
                                        fd.filename,
                                        SF::new(fd.len),
                                        if included { "" } else { ", will skip" }
                                    )
                                }
                                continue;
                            }
                            AddTorrentResponse::Added(_, handle) => {
                                added = true;
                                handle
                            }
                        },
                        Err(err) => {
                            error!("error adding {:?}: {:?}", &path, err);
                            continue;
                        }
                    };

                    handles.push(handle);
                }

                if download_opts.list {
                    Ok(())
                } else if added {
                    if download_opts.exit_on_finish {
                        let results = futures::future::join_all(
                            handles.iter().map(|h| h.wait_until_completed()),
                        );
                        let results = tokio::select! {
                            _ = cancel.cancelled() => {
                                bail!("cancelled");
                            },
                            r = results => r
                        };
                        if results.iter().any(|r| r.is_err()) {
                            anyhow::bail!("some downloads failed")
                        }
                        info!("All downloads completed, exiting");
                        Ok(())
                    } else {
                        // Sleep forever.
                        cancel.cancelled().await;
                        bail!("cancelled");
                    }
                } else {
                    anyhow::bail!("no torrents were added")
                }
            }
        }
        SubCommand::Completions(completions_opts) => {
            clap_complete::generate(
                completions_opts.shell,
                &mut Opts::command(),
                "rqbit",
                &mut io::stdout(),
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn test_parse_umask() {
        use crate::parse_umask;
        let range = b'0'..=b'7';
        for d0 in range.clone() {
            for d1 in range.clone() {
                for d2 in range.clone() {
                    let inp = [d0, d1, d2];
                    let inp_str = std::str::from_utf8(&inp).unwrap();
                    let parsed = parse_umask(inp_str).expect(inp_str);
                    let expected = format!("{parsed:03o}");
                    assert_eq!(inp_str, expected);
                }
            }
        }
    }
}
