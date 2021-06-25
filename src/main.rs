use std::{fs::File, io::Read};

use anyhow::Context;
use clap::Clap;
use librqbit::{
    clone_to_owned::CloneToOwned, torrent_manager::TorrentManagerBuilder,
    torrent_metainfo::torrent_from_bytes,
};
use log::info;

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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    pretty_env_logger::init();

    let opts = Opts::parse();

    let torrent =
        if opts.torrent_path.starts_with("http://") || opts.torrent_path.starts_with("https://") {
            let response = reqwest::get(&opts.torrent_path).await.with_context(|| {
                format!(
                    "error downloading torrent metadata from {}",
                    &opts.torrent_path
                )
            })?;
            if !response.status().is_success() {
                anyhow::bail!("GET {} returned {}", &opts.torrent_path, response.status())
            }
            let b = response.bytes().await.with_context(|| {
                format!("error reading repsonse body from {}", &opts.torrent_path)
            })?;
            torrent_from_bytes(&b)
                .context("error decoding torrent")?
                .clone_to_owned()
        } else {
            let mut buf = Vec::new();
            if opts.torrent_path == "-" {
                std::io::stdin()
                    .read_to_end(&mut buf)
                    .context("error reading stdin")?;
            } else {
                File::open(&opts.torrent_path)
                    .with_context(|| format!("error opening {}", &opts.torrent_path))?
                    .read_to_end(&mut buf)
                    .with_context(|| format!("error reading {}", &opts.torrent_path))?;
            }
            torrent_from_bytes(&buf)
                .context("error decoding torrent")?
                .clone_to_owned()
        };

    info!("Torrent metadata: {:#?}", &torrent);

    let builder = TorrentManagerBuilder::new(torrent, opts.output_folder).overwrite(opts.overwrite);
    let manager_handle = builder.start_manager().await?;
    manager_handle.wait_until_completed().await?;
    Ok(())
}
