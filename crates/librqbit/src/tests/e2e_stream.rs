use std::{net::Ipv4Addr, time::Duration};

use anyhow::Context;
use tempfile::TempDir;
use tokio::{io::AsyncReadExt, time::timeout};
use tracing::info;

use crate::{
    AddTorrent, CreateTorrentOptions, Session, create_torrent,
    spawn_utils::BlockingSpawner,
    tests::test_util::{TestPeerMetadata, setup_test_logging},
};

use super::test_util::create_default_random_dir_with_torrents;

/// `idle_streams`: streams opened on the client before anything has been
/// downloaded, and kept open while the test stream reads the file.
async fn e2e_stream(listen_port: u16, idle_streams: usize) -> anyhow::Result<()> {
    setup_test_logging();
    let files = create_default_random_dir_with_torrents(1, 8192, Some("test_e2e_stream"));
    let torrent = create_torrent(
        files.path(),
        CreateTorrentOptions {
            name: None,
            piece_length: Some(1024),
            ..Default::default()
        },
        &BlockingSpawner::new(1),
    )
    .await?;

    let orig_content = std::fs::read(files.path().join("0.data")).unwrap();
    let server_session = Session::new_with_opts(
        files.path().into(),
        crate::SessionOptions {
            dht: None,
            peer_id: Some(TestPeerMetadata::good().as_peer_id()),
            persistence: None,
            listen: Some(crate::listen::ListenerOptions {
                listen_addr: (Ipv4Addr::LOCALHOST, listen_port).into(),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .context("error creating server session")?;

    info!("created server session");

    timeout(
        Duration::from_secs(5),
        server_session
            .add_torrent(
                AddTorrent::from_bytes(torrent.as_bytes()?),
                Some(crate::AddTorrentOptions {
                    paused: false,
                    output_folder: Some(files.path().to_str().unwrap().to_owned()),
                    overwrite: true,
                    ..Default::default()
                }),
            )
            .await?
            .into_handle()
            .unwrap()
            .wait_until_completed(),
    )
    .await?
    .context("error adding torrent")?;

    info!("server torrent was completed");

    let peer = server_session
        .listen_addr()
        .context("expected listen_addr to be set")?;

    let client_dir = TempDir::with_prefix("test_e2e_stream_client")?;

    let client_session = Session::new_with_opts(
        client_dir.path().into(),
        crate::SessionOptions {
            dht: None,
            persistence: None,
            peer_id: Some(TestPeerMetadata::good().as_peer_id()),
            ..Default::default()
        },
    )
    .await?;

    info!("created client session");

    let client_handle = client_session
        .add_torrent(
            AddTorrent::from_bytes(torrent.as_bytes()?),
            Some(crate::AddTorrentOptions {
                // Paused, so the idle streams open before any piece arrives.
                paused: idle_streams > 0,
                initial_peers: Some(vec![peer]),
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .unwrap();

    client_handle.wait_until_initialized().await?;

    let mut idle = Vec::with_capacity(idle_streams);
    for _ in 0..idle_streams {
        idle.push(client_handle.clone().stream(0).await?);
    }
    if idle_streams > 0 {
        client_session.unpause(&client_handle).await?;
    }

    info!("client torrent initialized, starting stream");

    let mut stream = client_handle.stream(0).await?;
    let mut buf = Vec::<u8>::with_capacity(8192);
    stream.read_to_end(&mut buf).await?;

    if buf != orig_content {
        panic!("contents differ")
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_e2e_stream() -> anyhow::Result<()> {
    timeout(Duration::from_secs(10), e2e_stream(16001, 0)).await?
}

// As many open streams as the session has blocking permits (8 by default).
// Received pieces must still get written, or every stream waits forever.
#[tokio::test(flavor = "multi_thread")]
async fn test_e2e_stream_as_many_streams_as_blocking_permits() -> anyhow::Result<()> {
    timeout(Duration::from_secs(10), e2e_stream(16002, 7)).await?
}
