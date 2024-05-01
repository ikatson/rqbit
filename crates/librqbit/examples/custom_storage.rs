use std::time::Duration;

use librqbit::{storage::mmap::MmapStorageFactory, SessionOptions};
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Output logs to console.
    match std::env::var("RUST_LOG") {
        Ok(_) => {}
        Err(_) => std::env::set_var("RUST_LOG", "info"),
    }
    tracing_subscriber::fmt::init();
    let s = librqbit::Session::new_with_opts(
        Default::default(),
        SessionOptions {
            disable_dht_persistence: true,
            persistence: false,
            listen_port_range: None,
            enable_upnp_port_forwarding: false,
            ..Default::default()
        },
    )
    .await?;
    let handle = s
        .add_torrent(
            librqbit::AddTorrent::TorrentFileBytes(
                include_bytes!("../resources/ubuntu-21.04-live-server-amd64.iso.torrent").into(),
            ),
            Some(librqbit::AddTorrentOptions {
                storage_factory: Some(Box::new(MmapStorageFactory {})),
                paused: false,
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .unwrap();
    tokio::spawn({
        let h = handle.clone();
        async move {
            loop {
                info!("{}", h.stats());
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    });
    handle.wait_until_completed().await?;
    Ok(())
}
