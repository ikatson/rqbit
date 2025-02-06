use std::{net::Ipv4Addr, path::PathBuf, time::Duration};

use bytes::Bytes;
use tracing::info;

use crate::{
    listen::ListenerOptions, tests::test_util::setup_test_logging, AddTorrentOptions,
    ConnectionOptions, Session, SessionOptions,
};

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_utp_with_another_client() {
    test_with_another_client(crate::SessionOptions {
        disable_dht: true,
        persistence: None,
        listen: Some(ListenerOptions {
            mode: crate::listen::ListenerMode::UtpOnly,
            listen_addr: (Ipv4Addr::LOCALHOST, 57318).into(),
            ..Default::default()
        }),
        connect: Some(ConnectionOptions {
            enable_tcp: false,
            ..Default::default()
        }),
        ..Default::default()
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_tcp_with_another_client() {
    test_with_another_client(crate::SessionOptions {
        disable_dht: true,
        persistence: None,
        listen: Some(ListenerOptions {
            mode: crate::listen::ListenerMode::TcpOnly,
            listen_addr: (Ipv4Addr::LOCALHOST, 57318).into(),
            ..Default::default()
        }),
        ..Default::default()
    })
    .await
}

// A test to download a canary file from another torrent client on 127.0.0.1:27312.
// Disabled, uncomment if developing / testing / benchmarking.
//
// The canary file is created from librqbit_utp, then served from the other torrent client:
// cargo run --release --example create_canary_file /tmp/canary_4096m 4096
async fn test_with_another_client(sopts: SessionOptions) {
    let test_filename = {
        let f = PathBuf::from(std::env::args().next().unwrap());
        f.read_link().unwrap_or(f)
    };
    if std::fs::exists("/tmp/rtest").unwrap() {
        std::fs::remove_file("/tmp/rtest").unwrap();
    }
    std::os::unix::fs::symlink(test_filename, "/tmp/rtest").unwrap();

    setup_test_logging();

    let t = std::fs::read("/tmp/canary_4096m.torrent").unwrap();

    let session = Session::new_with_opts("/tmp/utptest".into(), sopts)
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
