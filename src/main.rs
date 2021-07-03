use std::{fs::File, io::Read, time::Duration};

use anyhow::Context;
use clap::Clap;
use librqbit::{
    spawn_utils::BlockingSpawner,
    torrent_manager::TorrentManagerBuilder,
    torrent_metainfo::{torrent_from_bytes, TorrentMetaV1Owned},
};
use log::{info, warn};
use reqwest::Url;

async fn torrent_from_url(url: &str) -> anyhow::Result<TorrentMetaV1Owned> {
    let response = reqwest::get(url)
        .await
        .with_context(|| format!("error downloading torrent metadata from {}", url))?;
    if !response.status().is_success() {
        anyhow::bail!("GET {} returned {}", url, response.status())
    }
    let b = response
        .bytes()
        .await
        .with_context(|| format!("error reading repsonse body from {}", url))?;
    torrent_from_bytes(&b).context("error decoding torrent")
}

fn torrent_from_file(filename: &str) -> anyhow::Result<TorrentMetaV1Owned> {
    let mut buf = Vec::new();
    if filename == "-" {
        std::io::stdin()
            .read_to_end(&mut buf)
            .context("error reading stdin")?;
    } else {
        File::open(filename)
            .with_context(|| format!("error opening {}", filename))?
            .read_to_end(&mut buf)
            .with_context(|| format!("error reading {}", filename))?;
    }
    torrent_from_bytes(&buf).context("error decoding torrent")
}

#[derive(Debug, Clap)]
enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Clap)]
#[clap(version = "1.0", author = "Igor Katson <igor.katson@gmail.com>")]
struct Opts {
    /// The filename or URL of the .torrent file.
    torrent_path: String,

    /// The filename of the .torrent file.
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

    /// The interval in seconds to poll trackers.
    /// Trackers send the refresh interval when we connect to them. Often this is
    /// pretty big, e.g. 30 minutes. This can force a certain value.
    #[clap(short = 'i', long = "tracker-refresh-interval")]
    force_tracker_interval: Option<u64>,

    /// Set this flag if you want to use tokio's single threaded runtime.
    /// It MAY perform better, but the main purpose is easier debugging, as time
    /// profilers work better with this one.
    #[clap(short, long)]
    single_thread_runtime: bool,
}

fn compute_only_files(
    torrent: &TorrentMetaV1Owned,
    filename_re: &str,
) -> anyhow::Result<Vec<usize>> {
    let filename_re = regex::Regex::new(&filename_re).context("filename regex is incorrect")?;
    let mut only_files = Vec::new();
    for (idx, (filename, _)) in torrent.info.iter_filenames_and_lengths().enumerate() {
        let full_path = filename
            .to_pathbuf()
            .with_context(|| format!("filename of file {} is not valid utf8", idx))?;
        if filename_re.is_match(
            full_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("filename of file {} is not valid utf8", idx))?,
        ) {
            only_files.push(idx);
        }
    }
    if only_files.is_empty() {
        anyhow::bail!("none of the filenames match the given regex")
    }
    Ok(only_files)
}

fn init_logging(opts: &Opts) {
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
            if std::env::var_os("RUST_LOG").is_none() {
                std::env::set_var("RUST_LOG", "info");
            };
        }
    };
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

    rt.block_on(async move {
        let torrent = if opts.torrent_path.starts_with("http://")
            || opts.torrent_path.starts_with("https://")
        {
            torrent_from_url(&opts.torrent_path).await?
        } else {
            torrent_from_file(&opts.torrent_path)?
        };

        info!("Torrent metadata: {:#?}", &torrent);
        if opts.list {
            return Ok(());
        }

        let only_files = if let Some(filename_re) = opts.only_files_matching_regex {
            Some(compute_only_files(&torrent, &filename_re)?)
        } else {
            None
        };

        let trackers = torrent
            .iter_announce()
            .filter_map(|tracker| {
                let url = match std::str::from_utf8(tracker.as_ref()) {
                    Ok(url) => url,
                    Err(_) => {
                        warn!("cannot parse tracker url as utf-8, ignoring");
                        return None;
                    }
                };
                match Url::parse(url) {
                    Ok(url) => Some(url),
                    Err(e) => {
                        warn!("cannot parse tracker URL {}: {}", url, e);
                        None
                    }
                }
            })
            .collect::<Vec<_>>();

        let mut builder =
            TorrentManagerBuilder::new(torrent.info, torrent.info_hash, opts.output_folder);
        builder.overwrite(opts.overwrite).spawner(spawner);
        if let Some(only_files) = only_files {
            builder.only_files(only_files);
        }

        if let Some(interval) = opts.force_tracker_interval {
            builder.force_tracker_interval(Duration::from_secs(interval));
        }

        let handle = builder.start_manager()?;

        for url in trackers {
            handle.add_tracker(url);
        }

        handle.wait_until_completed().await?;
        Ok(())
    })
}
