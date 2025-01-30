use std::time::Duration;

use bytes::Bytes;
use tempfile::TempDir;
use tracing::info;

use crate::{tests::test_util::setup_test_logging, AddTorrentOptions, Session};

#[tokio::test]
async fn test_utp_with_another_client() {
    setup_test_logging();

    let t = include_bytes!("../../resources/test/random.torrent");

    let session = Session::new_with_opts(
        "/tmp/utptest".into(),
        crate::SessionOptions {
            disable_dht: true,
            persistence: None,
            listen_port_range: None,
            enable_upnp_port_forwarding: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let handle = session
        .add_torrent(
            crate::AddTorrent::TorrentFileBytes(Bytes::from_static(t)),
            Some(AddTorrentOptions {
                overwrite: true,
                initial_peers: Some(vec!["127.0.0.1:27312".parse().unwrap()]),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .into_handle()
        .unwrap();

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

    handle.wait_until_completed().await.unwrap();
}
