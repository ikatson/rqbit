use std::{net::SocketAddr, str::FromStr, sync::Arc, time::Duration};

use anyhow::Context;
use clap::Clap;
use librqbit::{
    http_api::HttpApi,
    peer_connection::PeerConnectionOptions,
    session::{AddTorrentOptions, Session, SessionOptions},
    spawn_utils::{spawn, BlockingSpawner},
};
use log::info;
use size_format::SizeFormatterBinary as SF;

#[derive(Debug, Clap)]
enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy)]
struct ParsedDuration(Duration);
impl FromStr for ParsedDuration {
    type Err = parse_duration::parse::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_duration::parse(s).map(ParsedDuration)
    }
}

#[derive(Clap)]
#[clap(version, author, about)]
struct Opts {
    /// The filename or URL of the torrent. If URL, http/https/magnet are supported.
    torrent_path: String,

    /// The output folder to write to. If not exists, it will be created.
    output_folder: String,

    /// If set, only the file whose filename matching this regex will
    /// be downloaded
    #[clap(short = 'r', long = "filename-re")]
    only_files_matching_regex: Option<String>,

    /// Set if you are ok to write on top of existing files
    #[clap(long)]
    overwrite: bool,

    /// Only list the torrent metadata contents, don't do anything else.
    #[clap(short, long)]
    list: bool,

    /// The loglevel
    #[clap(arg_enum, short = 'v')]
    log_level: Option<LogLevel>,

    /// The interval to poll trackers, e.g. 30s.
    /// Trackers send the refresh interval when we connect to them. Often this is
    /// pretty big, e.g. 30 minutes. This can force a certain value.
    #[clap(short = 'i', long = "tracker-refresh-interval")]
    force_tracker_interval: Option<ParsedDuration>,

    /// The listen address for HTTP API
    #[clap(long = "http-api-listen-addr", default_value = "127.0.0.1:3030")]
    http_api_listen_addr: SocketAddr,

    /// Set this flag if you want to use tokio's single threaded runtime.
    /// It MAY perform better, but the main purpose is easier debugging, as time
    /// profilers work better with this one.
    #[clap(short, long)]
    single_thread_runtime: bool,

    #[clap(long = "disable-dht")]
    disable_dht: bool,

    /// Set this to disable DHT reading and storing it's state.
    /// For now this is a useful workaround if you want to launch multiple rqbit instances,
    /// otherwise DHT port will conflict.
    #[clap(long = "disable-dht-persistence")]
    disable_dht_persistence: bool,

    /// The connect timeout, e.g. 1s, 1.5s, 100ms etc.
    #[clap(long = "peer-connect-timeout")]
    peer_connect_timeout: Option<ParsedDuration>,
}

fn init_logging(opts: &Opts) {
    if std::env::var_os("RUST_LOG").is_none() {
        match opts.log_level.as_ref() {
            Some(level) => {
                let level_str = match level {
                    LogLevel::Trace => "trace",
                    LogLevel::Debug => "debug",
                    LogLevel::Info => "info",
                    LogLevel::Warn => "warn",
                    LogLevel::Error => "error",
                };
                std::env::set_var("RUST_LOG", level_str);
            }
            None => {
                std::env::set_var("RUST_LOG", "info");
            }
        };
    }
    pretty_env_logger::init();
}

fn main() -> anyhow::Result<()> {
    let opts = Opts::parse();

    init_logging(&opts);

    let (mut rt_builder, spawner) = match opts.single_thread_runtime {
        true => (
            tokio::runtime::Builder::new_current_thread(),
            BlockingSpawner::new(false),
        ),
        false => (
            tokio::runtime::Builder::new_multi_thread(),
            BlockingSpawner::new(true),
        ),
    };

    let rt = rt_builder
        .enable_time()
        .enable_io()
        // the default is 512, it can get out of hand, as this program is CPU-bound on
        // hash checking.
        // note: we aren't using spawn_blocking() anymore, so this doesn't apply,
        // however I'm still messing around, so in case we do, let's block the number of
        // spawned threads.
        .max_blocking_threads(8)
        .build()?;

    rt.block_on(async_main(opts, spawner))
}

async fn async_main(opts: Opts, spawner: BlockingSpawner) -> anyhow::Result<()> {
    let sopts = SessionOptions {
        disable_dht: opts.disable_dht,
        disable_dht_persistence: opts.disable_dht_persistence,
        dht_config: None,
        peer_id: None,
        peer_opts: Some(PeerConnectionOptions {
            connect_timeout: opts.peer_connect_timeout.map(|d| d.0),
            ..Default::default()
        }),
    };

    let session = Arc::new(
        Session::new_with_opts(opts.output_folder.into(), spawner, sopts)
            .await
            .context("error initializing rqbit session")?,
    );

    let torrent_opts = AddTorrentOptions {
        only_files_regex: opts.only_files_matching_regex,
        overwrite: opts.overwrite,
        list_only: opts.list,
        force_tracker_interval: opts.force_tracker_interval.map(|d| d.0),
        ..Default::default()
    };

    let handle = match session
        .add_torrent(opts.torrent_path, Some(torrent_opts))
        .await
        .context("error adding torrent to session")?
    {
        Some(handle) => handle,
        None => return Ok(()),
    };

    {
        let http_api = HttpApi::new(session.clone());
        http_api.add_mgr(handle.clone());
        spawn("HTTP API", {
            let http_api_listen_addr = opts.http_api_listen_addr;
            async move { http_api.make_http_api_and_run(http_api_listen_addr).await }
        });
    };

    spawn("Stats printer", {
        let handle = handle.clone();
        async move {
            loop {
                let peer_stats = handle.torrent_state().peer_stats_snapshot();
                let stats = handle.torrent_state().stats_snapshot();
                let speed = handle.speed_estimator();
                let total = stats.total_bytes;
                let progress = stats.total_bytes - stats.remaining_bytes;
                let downloaded_pct = if stats.remaining_bytes == 0 {
                    100f64
                } else {
                    (progress as f64 / total as f64) * 100f64
                };
                info!(
                    "Stats: {:.2}% ({:.2}), down speed {:.2} Mbps, fetched {}, remaining {:.2} of {:.2}, uploaded {:.2}, peers: {{live: {}, connecting: {}, queued: {}, seen: {}}}",
                    downloaded_pct,
                    SF::new(progress),
                    speed.download_mbps(),
                    SF::new(stats.fetched_bytes),
                    SF::new(stats.remaining_bytes),
                    SF::new(total),
                    SF::new(stats.uploaded_bytes),
                    peer_stats.live,
                    peer_stats.connecting,
                    peer_stats.queued,
                    peer_stats.seen,
                );
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    });

    handle
        .wait_until_completed()
        .await
        .context("error waiting for torrent completion")?;
    Ok(())
}
