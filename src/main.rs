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

#[derive(Clap)]
#[clap(version = "1.0", author = "Igor Katson <igor.katson@gmail.com>")]
struct Opts {
    /// The filename or URL of the .torrent file.
    torrent_path: String,

    /// The filename of the .torrent file.
    output_folder: String,

    /// Set if you are ok to write on top of existing files
    #[clap(long)]
    overwrite: bool,
}

fn main() -> anyhow::Result<()> {
    pretty_env_logger::init();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        // the default is 512, it can get out of hand.
        .max_blocking_threads(8)
        .build()?;

    rt.block_on(async move {
        let opts = Opts::parse();

        let torrent = if opts.torrent_path.starts_with("http://")
            || opts.torrent_path.starts_with("https://")
        {
            torrent_from_url(&opts.torrent_path).await?
        } else {
            torrent_from_file(&opts.torrent_path)?
        };

        info!("Torrent metadata: {:#?}", &torrent);

        let builder =
            TorrentManagerBuilder::new(torrent, opts.output_folder).overwrite(opts.overwrite);
        let manager_handle = builder.start_manager().await?;
        manager_handle.wait_until_completed().await?;
        Ok(())
    })
}
