// For production-grade code look at rqbit::main(), which does the same but has more options.
//
// Usage:
// cargo run --release --example ubuntu /tmp/ubuntu/

use std::time::Duration;

use anyhow::Context;
use librqbit::session::{AddTorrentOptions, AddTorrentResponse, Session};
use tracing::info;

// This is ubuntu-21.04-live-server-amd64.iso.torrent
// You can also pass filenames and URLs to add_torrent().
const MAGNET_LINK: &str = "magnet:?xt=urn:btih:cab507494d02ebb1178b38f2e9d7be299c86b862";

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    // Output logs to console.
    tracing_subscriber::fmt::init();

    let output_dir = std::env::args()
        .nth(1)
        .expect("the first argument should be the output directory");

    // Create the session
    let session = Session::new(output_dir.into(), Default::default())
        .await
        .context("error creating session")?;

    // Add the torrent to the session
    let handle = match session
        .add_torrent(
            MAGNET_LINK,
            Some(AddTorrentOptions {
                // Set this to true to allow writing on top of existing files.
                // If the file is partially downloaded, librqbit will only download the
                // missing pieces.
                //
                // Otherwise it will throw an error that the file exists.
                overwrite: false,
                ..Default::default()
            }),
        )
        .await
        .context("error adding torrent")?
    {
        AddTorrentResponse::Added(handle) => handle,
        // For a brand new session other variants won't happen.
        _ => unreachable!(),
    };

    // Print stats periodically.
    tokio::spawn({
        let handle = handle.clone();
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let stats = handle.torrent_state().stats_snapshot();
                info!("stats: {stats:?}");
            }
        }
    });

    // Wait until the download is completed
    handle.wait_until_completed().await?;
    info!("torrent downloaded");

    Ok(())
}
