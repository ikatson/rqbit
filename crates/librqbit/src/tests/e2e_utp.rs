use std::{net::Ipv4Addr, path::PathBuf, time::Duration};

use bytes::Bytes;
use tracing::info;

use crate::{
    listen::ListenerOptions, tests::test_util::setup_test_logging, AddTorrentOptions, Session,
};

#[tokio::test(flavor = "multi_thread")]
async fn test_utp_with_another_client() {
    if cfg!(all(test, not(debug_assertions))) {
        let test_filename = {
            let f = PathBuf::from(std::env::args().next().unwrap());
            f.read_link().unwrap_or(f)
        };
        if std::fs::exists("/tmp/rtest").unwrap() {
            std::fs::remove_file("/tmp/rtest").unwrap();
        }
        std::os::unix::fs::symlink(test_filename, "/tmp/rtest").unwrap();
    }

    setup_test_logging();

    // let t = include_bytes!("/tmp/canary_16m.torrent");
    // let t = include_bytes!("/tmp/canary_128m.torrent");
    // let t = include_bytes!("/tmp/canary_512m.torrent");
    let t = std::fs::read("/tmp/canary_4096m.torrent").unwrap();

    let session = Session::new_with_opts(
        "/tmp/utptest".into(),
        crate::SessionOptions {
            disable_dht: true,
            persistence: None,
            listen: Some(ListenerOptions {
                mode: crate::listen::ListenerMode::UtpOnly,
                listen_addr: (Ipv4Addr::LOCALHOST, 57318).into(),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let handle = session
        .add_torrent(
            crate::AddTorrent::TorrentFileBytes(Bytes::from(t)),
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
                tokio::time::sleep(Duration::from_millis(100)).await;
                let stats = handle.stats();
                info!("{stats:}");
            }
        }
    });

    handle.wait_until_completed().await.unwrap();
}
