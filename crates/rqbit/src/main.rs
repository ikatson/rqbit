use std::{io, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{bail, Context};
use clap::{CommandFactory, Parser, ValueEnum};
use clap_complete::Shell;
use librqbit::{
    api::ApiAddTorrentResponse,
    http_api::{HttpApi, HttpApiOptions},
    http_api_client, librqbit_spawn,
    storage::{
        filesystem::{FilesystemStorageFactory, MmapFilesystemStorageFactory},
        StorageFactory, StorageFactoryExt,
    },
    tracing_subscriber_config_utils::{init_logging, InitLoggingOptions},
    AddTorrent, AddTorrentOptions, AddTorrentResponse, Api, ListOnlyResponse,
    PeerConnectionOptions, Session, SessionOptions, SessionPersistenceConfig, TorrentStatsState,
};
use size_format::SizeFormatterBinary as SF;
use tokio::net::TcpListener;
use tracing::{error, error_span, info, trace_span, warn};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Parser)]
#[command(version, author, about)]
struct Opts {
    /// The console loglevel
    #[arg(value_enum, short = 'v')]
    log_level: Option<LogLevel>,

    /// The log filename to also write to in addition to the console.
    #[arg(long = "log-file")]
    log_file: Option<String>,

    /// The value for RUST_LOG in the log file
    #[arg(long = "log-file-rust-log", default_value = "librqbit=debug,info")]
    log_file_rust_log: String,

    /// The interval to poll trackers, e.g. 30s.
    /// Trackers send the refresh interval when we connect to them. Often this is
    /// pretty big, e.g. 30 minutes. This can force a certain value.
    #[arg(short = 'i', long = "tracker-refresh-interval", value_parser = parse_duration::parse)]
    force_tracker_interval: Option<Duration>,

    /// The listen address for HTTP API
    #[arg(long = "http-api-listen-addr", default_value = "127.0.0.1:3030")]
    http_api_listen_addr: SocketAddr,

    /// Set this flag if you want to use tokio's single threaded runtime.
    /// It MAY perform better, but the main purpose is easier debugging, as time
    /// profilers work better with this one.
    #[arg(short, long)]
    single_thread_runtime: bool,

    #[arg(long = "disable-dht")]
    disable_dht: bool,

    /// Set this to disable DHT reading and storing it's state.
    /// For now this is a useful workaround if you want to launch multiple rqbit instances,
    /// otherwise DHT port will conflict.
    #[arg(long = "disable-dht-persistence")]
    disable_dht_persistence: bool,

    /// The connect timeout, e.g. 1s, 1.5s, 100ms etc.
    #[arg(long = "peer-connect-timeout", value_parser = parse_duration::parse, default_value="2s")]
    peer_connect_timeout: Duration,

    /// The connect timeout, e.g. 1s, 1.5s, 100ms etc.
    #[arg(long = "peer-read-write-timeout" , value_parser = parse_duration::parse, default_value="10s")]
    peer_read_write_timeout: Duration,

    /// How many threads to spawn for the executor.
    #[arg(short = 't', long)]
    worker_threads: Option<usize>,

    // Enable to listen on 0.0.0.0 on TCP for torrent requests.
    #[arg(long = "disable-tcp-listen")]
    disable_tcp_listen: bool,

    /// The minimal port to listen for incoming connections.
    #[arg(long = "tcp-min-port", default_value = "4240")]
    tcp_listen_min_port: u16,

    /// The maximal port to listen for incoming connections.
    #[arg(long = "tcp-max-port", default_value = "4260")]
    tcp_listen_max_port: u16,

    /// If set, will try to publish the chosen port through upnp on your router.
    #[arg(long = "disable-upnp")]
    disable_upnp: bool,

    /// If set, will run a UPNP Media server and stream all the torrents through it.
    /// Should be set to your hostname/IP as seen by your LAN neighbors.
    #[arg(long = "upnp-server-hostname")]
    upnp_server_hostname: Option<String>,

    /// UPNP server name that would be displayed on devices in your network.
    #[arg(long = "upnp-server-friendly-name")]
    upnp_server_friendly_name: Option<String>,

    #[command(subcommand)]
    subcommand: SubCommand,

    /// How many maximum blocking tokio threads to spawn to process disk reads/writes.
    /// This will indicate how many parallel reads/writes can happen at a moment in time.
    /// The higher the number, the more the memory usage.
    #[arg(long = "max-blocking-threads", default_value = "8")]
    max_blocking_threads: u16,

    // If you set this to something, all writes to disk will happen in background and be
    // buffered in memory up to approximately the given number of megabytes.
    //
    // Might be useful for slow disks.
    #[arg(long = "defer-writes-up-to")]
    defer_writes_up_to: Option<usize>,

    /// Use mmap (file-backed) for storage. Any advantages are questionable and unproven.
    /// If you use it, you know what you are doing.
    #[arg(long)]
    experimental_mmap_storage: bool,

    /// Provide a socks5 URL.
    /// The format is socks5://[username:password]@host:port
    ///
    /// Alternatively, set this as an environment variable RQBIT_SOCKS_PROXY_URL
    #[arg(long)]
    socks_url: Option<String>,

    /// How many torrents can be initializing (rehashing) at the same time
    #[arg(long, default_value = "5")]
    concurrent_init_limit: usize,
}

#[derive(Parser)]
struct ServerStartOptions {
    /// The output folder to write to. If not exists, it will be created.
    output_folder: String,
    #[arg(
        long = "disable-persistence",
        help = "Disable server persistence. It will not read or write its state to disk."
    )]

    /// Disable session persistence.
    disable_persistence: bool,

    /// The folder to store session data in. By default uses OS specific folder.
    #[arg(long = "persistence-config")]
    persistence_config: Option<String>,

    /// [Experimental] if set, will try to resume quickly after restart and skip checksumming.
    #[arg(long = "fastresume")]
    fastresume: bool,
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

    rt.block_on(async_main(opts))
}

async fn async_main(opts: Opts) -> anyhow::Result<()> {
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
        Ok(limit) => info!(limit = limit, "inreased open file limit"),
        Err(e) => warn!("failed increasing open file limit: {:#}", e),
    };

    let socks_url = opts
        .socks_url
        .or_else(|| std::env::var("RQBIT_SOCKS_PROXY_URL").ok());

    let mut sopts = SessionOptions {
        disable_dht: opts.disable_dht,
        disable_dht_persistence: opts.disable_dht_persistence,
        dht_config: None,
        // This will be overriden by "server start" below if needed.
        persistence: None,
        peer_id: None,
        peer_opts: Some(PeerConnectionOptions {
            connect_timeout: Some(opts.peer_connect_timeout),
            read_write_timeout: Some(opts.peer_read_write_timeout),
            ..Default::default()
        }),
        listen_port_range: if !opts.disable_tcp_listen {
            Some(opts.tcp_listen_min_port..opts.tcp_listen_max_port)
        } else {
            None
        },
        enable_upnp_port_forwarding: !opts.disable_upnp,
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
        socks_proxy_url: socks_url,
        concurrent_init_limit: Some(opts.concurrent_init_limit),
        root_span: None,
        fastresume: false,
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
                    if let Some(p) = start_opts.persistence_config.as_ref() {
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
                    match opts.upnp_server_hostname {
                        Some(hn) => {
                            if opts.http_api_listen_addr.ip().is_loopback() {
                                bail!("cannot enable UPNP server as HTTP API listen addr is localhost. Change --http-api-listen-addr to start with 0.0.0.0");
                            }
                            let server = session
                                .make_upnp_adapter(
                                    opts.upnp_server_friendly_name
                                        .unwrap_or_else(|| format!("rqbit at {hn}")),
                                    hn,
                                    opts.http_api_listen_addr.port(),
                                )
                                .await
                                .context("error starting UPNP server")?;
                            Some(server)
                        }
                        None => None,
                    }
                };

                let api = Api::new(
                    session,
                    Some(log_config.rust_log_reload_tx),
                    Some(log_config.line_broadcast),
                );
                let http_api = HttpApi::new(api, Some(HttpApiOptions { read_only: false }));
                let http_api_listen_addr = opts.http_api_listen_addr;

                info!("starting HTTP API at http://{http_api_listen_addr}");
                let tcp_listener = TcpListener::bind(http_api_listen_addr)
                    .await
                    .with_context(|| format!("error binding to {http_api_listen_addr}"))?;

                let upnp_router = upnp_server.as_mut().and_then(|s| s.take_router().ok());
                let http_api_fut = http_api.make_http_api_and_run(tcp_listener, upnp_router);

                let res = match upnp_server {
                    Some(srv) => {
                        let upnp_fut = srv.run_ssdp_forever();

                        tokio::pin!(http_api_fut);
                        tokio::pin!(upnp_fut);

                        tokio::select! {
                            r = &mut http_api_fut => r,
                            r = &mut upnp_fut => r
                        }
                    }
                    None => http_api_fut.await,
                };

                res.context("error running rqbit server")
            }
        },
        SubCommand::Download(download_opts) => {
            if download_opts.torrent_path.is_empty() {
                anyhow::bail!("you must provide at least one URL to download")
            }
            let http_api_url = format!("http://{}", opts.http_api_listen_addr);
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
                            for file in details.files {
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
                let http_api = HttpApi::new(api, Some(HttpApiOptions { read_only: true }));
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
                                for (idx, (filename, len)) in
                                    info.iter_filenames_and_lengths()?.enumerate()
                                {
                                    let included = match &only_files {
                                        Some(files) => files.contains(&idx),
                                        None => true,
                                    };
                                    info!(
                                        "File {}, size {}{}",
                                        filename.to_string()?,
                                        SF::new(len),
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
                        )
                        .await;
                        if results.iter().any(|r| r.is_err()) {
                            anyhow::bail!("some downloads failed")
                        }
                        info!("All downloads completed, exiting");
                        Ok(())
                    } else {
                        // Sleep forever.
                        loop {
                            tokio::time::sleep(Duration::from_secs(60)).await;
                        }
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
