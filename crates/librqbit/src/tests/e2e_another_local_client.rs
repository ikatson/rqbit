use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    time::Duration,
};

use bytes::Bytes;
use tracing::info;

use crate::{
    listen::ListenerOptions, tests::test_util::setup_test_logging, AddTorrentOptions,
    ConnectionOptions, Session, SessionOptions,
};

// Create this from librqbit_utp: cargo run --release --example create_canary_file /tmp/canary_4096m 4096
const TORRENT_FILENAME: &str = "/tmp/canary.torrent";

// Where to download
const OUTPUT_FOLDER: &str = "/tmp/utptest";

// It's hard to find the binary in target/.../deps/librqbit*, so symlink itself
// here for easy profiling.
const BINARY_SYMLINK: &str = "/tmp/rtest";

const DEFAULT_LISTEN_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 57318);

// Serve the above file from the other client.
const DEFAULT_OTHER_CLIENT_ADDR: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 27312);

fn parse_listen_and_client_addr() -> (SocketAddr, SocketAddr) {
    let listen =
        std::env::var("E2E_LISTEN_ADDR").map_or(DEFAULT_LISTEN_ADDR, |v| v.parse().unwrap());
    let client =
        std::env::var("E2E_CLIENT_ADDR").map_or(DEFAULT_OTHER_CLIENT_ADDR, |v| v.parse().unwrap());
    (listen, client)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_utp_with_another_client() {
    let (listen_addr, client_addr) = parse_listen_and_client_addr();
    test_with_another_client(
        crate::SessionOptions {
            disable_dht: true,
            persistence: None,
            listen: Some(ListenerOptions {
                mode: crate::listen::ListenerMode::UtpOnly,
                listen_addr,
                ..Default::default()
            }),
            connect: Some(ConnectionOptions {
                enable_tcp: false,
                ..Default::default()
            }),
            ..Default::default()
        },
        client_addr,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn test_tcp_with_another_client() {
    let (listen_addr, client_addr) = parse_listen_and_client_addr();
    test_with_another_client(
        crate::SessionOptions {
            disable_dht: true,
            persistence: None,
            listen: Some(ListenerOptions {
                mode: crate::listen::ListenerMode::TcpOnly,
                listen_addr,
                ..Default::default()
            }),
            ..Default::default()
        },
        client_addr,
    )
    .await
}

// A test to download a canary file from another torrent client on 127.0.0.1:27312.
// Disabled, uncomment if developing / testing / benchmarking.
//
// The canary file is created from librqbit_utp, then served from the other torrent client:
// cargo run --release --example create_canary_file /tmp/canary_4096m 4096
async fn test_with_another_client(sopts: SessionOptions, addr: SocketAddr) {
    let test_filename = {
        let f = PathBuf::from(std::env::args().next().unwrap());
        f.read_link().unwrap_or(f)
    };

    #[cfg(unix)]
    {
        if std::fs::exists(BINARY_SYMLINK).unwrap() {
            std::fs::remove_file(BINARY_SYMLINK).unwrap();
        }
        std::os::unix::fs::symlink(test_filename, BINARY_SYMLINK).unwrap();
    }

    setup_test_logging();

    let t = std::fs::read(TORRENT_FILENAME).unwrap();

    let session = Session::new_with_opts(OUTPUT_FOLDER.into(), sopts)
        .await
        .unwrap();

    let handle = session
        .add_torrent(
            crate::AddTorrent::TorrentFileBytes(Bytes::from(t)),
            Some(AddTorrentOptions {
                overwrite: true,
                initial_peers: Some(vec![addr]),
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

    // wait for all final FIN-ACKs to be sent
    tokio::time::sleep(Duration::from_millis(100)).await;
}
