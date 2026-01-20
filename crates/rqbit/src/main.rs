use std::{
    collections::HashSet,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::Duration,
};

use anyhow::{Context, bail};
use clap::{CommandFactory, Parser, ValueEnum};
use clap_complete::Shell;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, Api, ConnectionOptions,
    CreateTorrentOptions, ListOnlyResponse, ListenerMode, ListenerOptions, PeerConnectionOptions,
    Session, SessionOptions, SessionPersistenceConfig, TorrentStatsState,
    http_api::{HttpApi, HttpApiOptions},
    librqbit_spawn,
    limits::LimitsConfig,
    storage::{
        StorageFactory, StorageFactoryExt,
        filesystem::{FilesystemStorageFactory, MmapFilesystemStorageFactory},
    },
    tracing_subscriber_config_utils::{InitLoggingOptions, InitLoggingResult, init_logging},
};
use librqbit_dualstack_sockets::TcpListener;
use size_format::SizeFormatterBinary as SF;
use tokio_util::sync::CancellationToken;
use tracing::{debug_span, error, info, trace_span, warn};

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

    /// The listen address for HTTP API.
    ///
    /// If not set, "rqbit server" will listen on 127.0.0.1:3030, and "rqbit download" will listen
    /// on an ephemeral port that it will print.
    #[arg(long = "http-api-listen-addr", env = "RQBIT_HTTP_API_LISTEN_ADDR")]
    http_api_listen_addr: Option<SocketAddr>,

    /// Allow creating torrents via HTTP API
    #[arg(long = "http-api-allow-create", env = "RQBIT_HTTP_API_ALLOW_CREATE")]
    http_api_allow_create: bool,

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

    /// Set DHT bootstrap addrs
    /// A comma separated list of host:port or ip:port
    #[arg(long = "dht-bootstrap-addrs", env = "RQBIT_DHT_BOOTSTRAP")]
    dht_bootstrap_addrs: Option<String>,

    /// The connect timeout, e.g. 1s, 1.5s, 100ms etc.
    #[arg(long = "peer-connect-timeout", value_parser = parse_duration::parse, default_value="2s", env="RQBIT_PEER_CONNECT_TIMEOUT")]
    peer_connect_timeout: Duration,

    /// The timeout for read() and write() operations, e.g. 1s, 1.5s, 100ms etc.
    #[arg(long = "peer-read-write-timeout" , value_parser = parse_duration::parse, default_value="10s", env="RQBIT_PEER_READ_WRITE_TIMEOUT")]
    peer_read_write_timeout: Duration,

    /// The maximum number of connected peers per torrent.
    #[arg(long = "peer-limit", env = "RQBIT_PEER_LIMIT")]
    peer_limit: Option<usize>,

    /// How many threads to spawn for the executor.
    #[arg(short = 't', long, env = "RQBIT_RUNTIME_WORKER_THREADS")]
    worker_threads: Option<usize>,

    /// Disable listening for incoming connections over TCP. Note that outgoing connections
    /// can still be made (--disable-tcp-connect to disable).
    #[arg(long = "disable-tcp-listen", env = "RQBIT_TCP_LISTEN_DISABLE")]
    disable_tcp_listen: bool,

    /// Disable outgoing connections over TCP.
    /// Note that listening over TCP for incoming connections is enabled by default
    /// (--disable-tcp-listen to disable).
    #[arg(long = "disable-tcp-connect", env = "RQBIT_TCP_CONNECT_DISABLE")]
    disable_tcp_connect: bool,

    /// Enable to listen and connect over uTP
    #[arg(
        long = "experimental-enable-utp-listen",
        env = "RQBIT_EXPERIMENTAL_UTP_LISTEN_ENABLE"
    )]
    enable_utp_listen: bool,

    /// The port to listen for incoming connections (applies to both TCP and uTP).
    ///
    /// Defaults to 4240 for the server, and an ephemeral port for "rqbit download / rqbit share".
    #[arg(long = "listen-port", env = "RQBIT_LISTEN_PORT")]
    listen_port: Option<u16>,

    /// The port to advertise to trackers and DHT.
    ///
    /// If not set, will be the same as listen-port.
    #[arg(long = "announce-port", env = "RQBIT_ANNOUNCE_PORT")]
    announce_port: Option<u16>,

    /// What's the IP to listen on. Default is to listen on all interfaces on IPv4 and IPv6.
    #[arg(long = "listen-ip", default_value = "::", env = "RQBIT_LISTEN_IP")]
    listen_ip: IpAddr,

    /// By default, rqbit will try to publish LISTEN_PORT through UPnP on your router.
    /// This can disable it.
    #[arg(
        long = "disable-upnp-port-forward",
        env = "RQBIT_UPNP_PORT_FORWARD_DISABLE"
    )]
    disable_upnp_port_forward: bool,

    /// If set, will run a UPnP Media server on RQBIT_HTTP_API_LISTEN_ADDR.
    #[arg(long = "enable-upnp-server", env = "RQBIT_UPNP_SERVER_ENABLE")]
    enable_upnp_server: bool,

    /// UPnP server name that would be displayed on devices in your network.
    #[arg(
        long = "upnp-server-friendly-name",
        env = "RQBIT_UPNP_SERVER_FRIENDLY_NAME"
    )]
    upnp_server_friendly_name: Option<String>,
    /// What network device to bind to for DHT, BT-UDP, BT-TCP, trackers and LSD.
    /// On OSX will use IP(V6)_BOUND_IF, on Linux will use SO_BINDTODEVICE.
    ///
    /// Not supported on Windows (will error if you try to use it).
    #[arg(long = "bind-device", env = "RQBIT_BIND_DEVICE")]
    bind_device_name: Option<String>,

    /// Force IPv4 only.
    #[arg(long = "ipv4-only", env = "RQBIT_IPV4_ONLY")]
    ipv4_only: bool,

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

    /// Use mmap (file-backed) for storage. Any advantages are questionable and unproven.
    /// If you use it, you know what you are doing.
    #[arg(long)]
    experimental_mmap_storage: bool,

    /// If set will use socks5 proxy for all outgoing connections.
    /// The format is socks5://[username:password]@host:port
    ///
    /// You may also want to disable incoming connections via --disable-tcp-listen.
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

    /// Limit download speed to bytes-per-second.
    #[arg(long = "ratelimit-download", env = "RQBIT_RATELIMIT_DOWNLOAD")]
    ratelimit_download_bps: Option<NonZeroU32>,

    /// Limit upload speed to bytes-per-second.
    #[arg(long = "ratelimit-upload", env = "RQBIT_RATELIMIT_UPLOAD")]
    ratelimit_upload_bps: Option<NonZeroU32>,

    /// Downloads a p2p blocklist from this url and blocks connections from/to those peers.
    /// Supports file:/// and http(s):// URLs. Format is newline-delimited "name:start_ip-end_ip"
    /// E.g. https://github.com/Naunter/BT_BlockLists/raw/refs/heads/master/bt_blocklists.gz
    #[arg(long, env = "RQBIT_BLOCKLIST_URL")]
    blocklist_url: Option<String>,

    /// Downloads a p2p allowlist from this url and blocks ALL connections BUT from/to those peers.
    /// Supports file:/// and http(s):// URLs. Format is newline-delimited "name:start_ip-end_ip"
    /// E.g. https://github.com/Naunter/BT_BlockLists/raw/refs/heads/master/bt_blocklists.gz
    #[arg(long, env = "RQBIT_ALLOWLIST_URL")]
    allowlist_url: Option<String>,

    /// The filename with tracker URLs to always use for each torrent. Newline-delimited.
    #[arg(long, env = "RQBIT_TRACKERS_FILENAME")]
    trackers_filename: Option<String>,

    /// Disable local peer discovery (LSD). By default rqbit will announce torrents to LAN.
    #[arg(long = "disable-lsd", env = "RQBIT_LSD_DISABLE")]
    disable_local_peer_discovery: bool,

    /// Disable trackers (for debugging DHT, LSD and --initial-peers)
    #[arg(long = "disable-trackers", env = "RQBIT_TRACKERS_DISABLE")]
    disable_trackers: bool,
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
    /// Start the server
    Start(ServerStartOptions),
}

#[derive(Parser)]
struct DownloadOpts {
    /// The filename or URL of the torrent. If URL, http/https/magnet are supported.
    torrent_path: Vec<String>,

    /// The output folder to write to. Defaults to current folder.
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

    /// A comma-separated list of initial peers
    #[arg(long = "initial-peers", value_parser = parse_initial_peers)]
    initial_peers: Option<SocketAddrList>,

    /// Disable HTTP API entirely.
    #[arg(long = "disable-http-api")]
    disable_http_api: bool,
}

#[derive(Clone)]
struct SocketAddrList(Vec<SocketAddr>);

fn parse_initial_peers(s: &str) -> anyhow::Result<SocketAddrList> {
    let mut v = Vec::<SocketAddr>::new();
    for addr in s.split(',') {
        v.push(
            addr.parse()
                .ok()
                .with_context(|| format!("invalid address {addr}, expected host:port"))?,
        );
    }
    Ok(SocketAddrList(v))
}

#[derive(Parser)]
struct CompletionsOpts {
    /// The shell to generate completions for
    shell: Shell,
}

#[derive(Parser)]
struct ShareOpts {
    /// The path to create and share a torrent from
    path: String,

    /// Optional torrent name to use in the torrent file and magnet.
    #[arg(short = 'n', long)]
    name: Option<String>,

    /// Tracker URLs to share to (comma separated). Will append these to trackers from RQBIT_TRACKERS_FILENAME.
    #[arg(value_delimiter = ',', num_args = 0..32)]
    trackers: Vec<url::Url>,
}

#[derive(Parser)]
enum SubCommand {
    /// Start rqbit server with HTTP API.
    Server(ServerOpts),
    /// Create a torrent from a given path and announce it. Stateless.
    Share(ShareOpts),
    /// Download a single torrent, stateless.
    Download(DownloadOpts),
    /// Shell completions. eval "$(rqbit completions bash)"
    Completions(CompletionsOpts),
}

fn main() -> anyhow::Result<()> {
    let opts = Opts::parse();

    if let SubCommand::Completions(completions_opts) = &opts.subcommand {
        clap_complete::generate(
            completions_opts.shell,
            &mut Opts::command(),
            "rqbit",
            &mut io::stdout(),
        );
        return Ok(());
    }

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
            let mut cancel_triggered = false;
            while let Some(sig) = signals.forever().next() {
                if cancel_triggered {
                    warn!("received signal {:?}, forcing shutdown", sig);
                    std::process::exit(1)
                }
                warn!("received signal {:?}, trying to shut down gracefully", sig);
                token.cancel();
                cancel_triggered = true;

                std::thread::spawn(|| {
                    std::thread::sleep(Duration::from_secs(5));
                    warn!("could not shutdown in time, killing myself");
                    std::process::exit(1)
                });
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

async fn async_main(mut opts: Opts, cancel: CancellationToken) -> anyhow::Result<()> {
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

    let trackers = if let Some(f) = &opts.trackers_filename {
        parse_trackers_file(f)
            .await
            .inspect_err(|e| warn!("error reading trackers file: {e:#}"))
            .unwrap_or_default()
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
        listen_addr: (opts.listen_ip, opts.listen_port.unwrap_or(0)).into(),
        enable_upnp_port_forwarding: !opts.disable_upnp_port_forward,
        announce_port: opts.announce_port,
        ipv4_only: opts.ipv4_only,
        ..Default::default()
    });

    let mut sopts = SessionOptions {
        disable_dht: opts.disable_dht,
        disable_dht_persistence: opts.disable_dht_persistence,
        dht_bootstrap_addrs: opts
            .dht_bootstrap_addrs
            .as_ref()
            .map(|s| s.split(",").map(|v| v.to_string()).collect()),
        dht_config: None,
        // This will be overridden by "server start" below if needed.
        persistence: None,
        peer_id: None,
        listen,
        connect: Some(ConnectionOptions {
            proxy_url: opts.socks_url.take(),
            enable_tcp: !opts.disable_tcp_connect,
            peer_opts: Some(PeerConnectionOptions {
                connect_timeout: Some(opts.peer_connect_timeout),
                read_write_timeout: Some(opts.peer_read_write_timeout),
                ..Default::default()
            }),
        }),
        bind_device_name: opts.bind_device_name.take(),
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
        blocklist_url: opts.blocklist_url.take(),
        allowlist_url: opts.allowlist_url.take(),
        disable_local_service_discovery: opts.disable_local_peer_discovery,
        disable_trackers: opts.disable_trackers,
        trackers,
        peer_limit: opts.peer_limit,
        runtime_worker_threads: Some(opts.max_blocking_threads as usize),
        ipv4_only: opts.ipv4_only,
    };

    #[allow(clippy::needless_update)]
    let mut http_api_opts = HttpApiOptions {
        read_only: true,
        basic_auth: if let Ok(up) = std::env::var("RQBIT_HTTP_BASIC_AUTH_USERPASS") {
            let (u, p) = up
                .split_once(":")
                .context("basic auth credentials should be in format username:password")?;
            Some((u.to_owned(), p.to_owned()))
        } else {
            None
        },
        allow_create: opts.http_api_allow_create,

        // We need to install prometheus recorder early before we registered any metrics.
        #[cfg(feature = "prometheus")]
        prometheus_handle: match metrics_exporter_prometheus::PrometheusBuilder::new()
            .install_recorder()
        {
            Ok(handle) => Some(handle),
            Err(e) => {
                warn!("error installing prometheus recorder: {e:#}");
                None
            }
        },

        ..Default::default()
    };

    match &opts.subcommand {
        SubCommand::Server(server_opts) => match &server_opts.subcommand {
            ServerSubcommand::Start(start_opts) => {
                // If the listen port wasn't set, default to 4240
                if let Some(l) = sopts.listen.as_mut()
                    && l.listen_addr.port() == 0
                {
                    l.listen_addr.set_port(4240);
                }

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

                http_api_opts.read_only = false;
                sopts.fastresume = start_opts.fastresume;

                let session =
                    Session::new_with_opts(PathBuf::from(&start_opts.output_folder), sopts)
                        .await
                        .context("error initializing rqbit session")?;
                spawn_stats_printer(session.clone());

                if let Some(watch_folder) = start_opts.watch_folder.as_ref() {
                    session.watch_folder(Path::new(watch_folder));
                }

                let http_api_fut = start_http_api(
                    cancel,
                    session.clone(),
                    opts.http_api_listen_addr
                        .unwrap_or((Ipv4Addr::LOCALHOST, 3030).into()),
                    http_api_opts,
                    &opts,
                    log_config,
                )
                .await?;

                http_api_fut.await
            }
        },
        SubCommand::Download(download_opts) => {
            if download_opts.torrent_path.is_empty() {
                anyhow::bail!("you must provide at least one URL to download")
            }

            // "rqbit download" is ephemeral, so disable all persistence.
            sopts.disable_dht_persistence = true;
            sopts.persistence = None;

            let mut disable_http_api = download_opts.disable_http_api;

            if download_opts.list {
                sopts.listen = None;
                disable_http_api = true;
            }

            if let Some(listen) = sopts.listen.as_mut() {
                // We are creating an ephemeral download, no point in port forwarding.
                listen.enable_upnp_port_forwarding = false;
            }

            let torrent_opts = || AddTorrentOptions {
                only_files_regex: download_opts.only_files_matching_regex.clone(),
                overwrite: download_opts.overwrite,
                list_only: download_opts.list,
                force_tracker_interval: opts.force_tracker_interval,
                sub_folder: download_opts.sub_folder.clone(),
                initial_peers: download_opts.initial_peers.as_ref().map(|p| &p.0).cloned(),
                disable_trackers: opts.disable_trackers,
                ..Default::default()
            };
            let session = Session::new_with_opts(
                download_opts
                    .output_folder
                    .as_ref()
                    .map(PathBuf::from)
                    .unwrap_or_default(),
                sopts,
            )
            .await
            .context("error initializing rqbit session")?;

            librqbit_spawn(
                trace_span!("stats_printer"),
                "stats_printer",
                stats_printer(session.clone()),
            );

            if !disable_http_api {
                let http_api_fut = start_http_api(
                    cancel.clone(),
                    session.clone(),
                    opts.http_api_listen_addr
                        .unwrap_or((Ipv4Addr::LOCALHOST, 0).into()),
                    http_api_opts,
                    &opts,
                    log_config,
                )
                .await?;
                librqbit_spawn(debug_span!("http_api"), "http_api", http_api_fut);
            }

            let mut added = false;
            let mut handles = Vec::new();

            for path in &download_opts.torrent_path {
                let handle = match session
                    .add_torrent(AddTorrent::from_cli_argument(path)?, Some(torrent_opts()))
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
                            for (idx, fd) in info.iter_file_details().enumerate() {
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
                    let results =
                        futures::future::join_all(handles.iter().map(|h| h.wait_until_completed()));
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
        SubCommand::Share(share_opts) => {
            if share_opts.path.is_empty() {
                anyhow::bail!("you must provide a path to share")
            }

            let path = PathBuf::from(&share_opts.path);
            if !path.exists() {
                anyhow::bail!("{path:?} does not exist")
            }

            // "rqbit share" is ephemeral, so disable all persistence.
            sopts.disable_dht_persistence = true;
            sopts.persistence = None;

            if sopts.listen.is_none() {
                anyhow::bail!("you disabled all listeners, can't share");
            }

            let trackers = sopts
                .trackers
                .iter()
                .chain(share_opts.trackers.iter())
                .map(|t| t.to_string())
                .collect();

            let session = Session::new_with_opts(PathBuf::new(), sopts)
                .await
                .context("error initializing rqbit session")?;

            let http_api_fut = start_http_api(
                cancel,
                session.clone(),
                opts.http_api_listen_addr
                    .unwrap_or((Ipv6Addr::UNSPECIFIED, 0).into()),
                http_api_opts,
                &opts,
                log_config,
            )
            .await?;

            let (create_result, _) = session
                .create_and_serve_torrent(
                    &path,
                    CreateTorrentOptions {
                        name: share_opts.name.as_deref(),
                        trackers,
                        ..Default::default()
                    },
                )
                .await
                .context("error creating and sharing torrent")?;

            spawn_stats_printer(session.clone());

            tracing::warn!(
                "WARNING: torrents are public, anyone can download it, even if they don't have the magnet link"
            );
            println!("share this magnet link: {}", create_result.as_magnet());

            http_api_fut.await
        }
        SubCommand::Completions(_) => unreachable!(),
    }
}

async fn start_http_api(
    cancel: CancellationToken,
    session: Arc<Session>,
    listen_addr: SocketAddr,
    http_api_opts: HttpApiOptions,
    opts: &Opts,
    log_config: InitLoggingResult,
) -> anyhow::Result<impl Future<Output = anyhow::Result<()>> + use<> + 'static> {
    let api = Api::new(
        session.clone(),
        Some(log_config.rust_log_reload_tx),
        Some(log_config.line_broadcast),
    );
    let http_api = HttpApi::new(api, Some(http_api_opts));
    let listener = TcpListener::bind_tcp(listen_addr, Default::default())
        .with_context(|| format!("error binding HTTP server to {listen_addr}"))?;
    let listen_addr = listener.bind_addr();
    info!("started HTTP API at http://{listen_addr}");

    let mut upnp_server = {
        match opts.enable_upnp_server {
            true => {
                if listen_addr.ip().is_loopback() {
                    bail!(
                        "cannot enable UPNP server as HTTP API listen addr is localhost. Change --http-api-listen-addr to start with 0.0.0.0"
                    );
                }
                let server = session
                    .make_upnp_adapter(
                        opts.upnp_server_friendly_name.clone().unwrap_or_else(|| {
                            format!("rqbit@{}", gethostname::gethostname().to_string_lossy())
                        }),
                        listen_addr.port(),
                    )
                    .await
                    .context("error starting UPNP server")?;
                Some(server)
            }
            false => None,
        }
    };

    let upnp_router = upnp_server.as_mut().and_then(|s| s.take_router().ok());
    let http_api_fut = http_api.make_http_api_and_run(listener, upnp_router);

    Ok(async move {
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
    })
}

async fn stats_printer(session: Arc<Session>) -> Result<(), &'static str> {
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
                        Some(d) => format!(", ETA: {d:?}"),
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
}

fn spawn_stats_printer(session: Arc<Session>) {
    librqbit_spawn(
        trace_span!("stats_printer"),
        "stats_printer",
        stats_printer(session.clone()),
    );
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
