use std::{fs::File, io::Read};

use anyhow::Context;
use clap::Clap;
use librqbit::{
    clone_to_owned::CloneToOwned,
    torrent_manager::TorrentManagerBuilder,
    torrent_metainfo::{torrent_from_bytes, TorrentMetaV1Owned},
};
use log::info;

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
    Ok(torrent_from_bytes(&b)
        .context("error decoding torrent")?
        .clone_to_owned())
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
    Ok(torrent_from_bytes(&buf)
        .context("error decoding torrent")?
        .clone_to_owned())
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

    #[clap(short = 'r', long = "filename-re")]
    only_files_matching_regex: Option<String>,

    /// Set if you are ok to write on top of existing files
    #[clap(long)]
    overwrite: bool,

    /// Only list the torrent metadata contents, don't do anything else.
    #[clap(short, long)]
    list: bool,

    #[clap(arg_enum, short = 'v')]
    log_level: Option<LogLevel>,
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

    let rt = tokio::runtime::Builder::new_multi_thread()
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

        let mut builder = TorrentManagerBuilder::new(torrent, opts.output_folder);
        builder.overwrite(opts.overwrite);
        if let Some(only_files) = only_files {
            builder.only_files(only_files);
        }

        let manager_handle = builder.start_manager().await?;
        manager_handle.wait_until_completed().await?;
        Ok(())
    })
}
