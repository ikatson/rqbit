// For production-grade code look at rqbit::main(), which does the same but has more options.
//
// Usage:
// cargo run --release --example ubuntu /tmp/ubuntu/

use std::time::Duration;

use anyhow::Context;
use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse, Session};
use tracing::info;

// This is ubuntu-21.04-live-server-amd64.iso.torrent
// You can also pass filenames and URLs to add_torrent().
const MAGNET_LINK: &str = "magnet:?xt=urn:btih:cab507494d02ebb1178b38f2e9d7be299c86b862";

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    // Output logs to console.
    match std::env::var("RUST_LOG") {
        Ok(_) => {}
        Err(_) => std::env::set_var("RUST_LOG", "info"),
    }
    tracing_subscriber::fmt::init();

    let output_dir = std::env::args()
        .nth(1)
        .expect("the first argument should be the output directory");

    // Create the session
    let session = Session::new(output_dir.into())
        .await
        .context("error creating session")?;

    // Add the torrent to the session
    let handle = match session
        .add_torrent(
            AddTorrent::from_url(MAGNET_LINK),
            Some(AddTorrentOptions {
                // Allow writing on top of existing files.
                overwrite: true,
                ..Default::default()
            }),
        )
        .await
        .context("error adding torrent")?
    {
        AddTorrentResponse::Added(_, handle) => handle,
        // For a brand new session other variants won't happen.
        _ => unreachable!(),
    };

    info!("Details: {:?}", &handle.info().info);

    // Print stats periodically.
    tokio::spawn({
        let handle = handle.clone();
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let stats = handle.stats();
                info!("{stats:}");
            }
        }
    });

    // Wait until the download is completed
    handle.wait_until_completed().await?;
    info!("torrent downloaded");

    Ok(())
}
