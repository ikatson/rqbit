use std::{net::SocketAddr, time::Duration};

use anyhow::Context;
use tokio::{io::AsyncReadExt, time::timeout};
use tracing::info;

use crate::{create_torrent, AddTorrent, CreateTorrentOptions, Session};

use super::test_util::create_default_random_dir_with_torrents;

async fn e2e_stream() -> anyhow::Result<()> {
    let files = create_default_random_dir_with_torrents(1, 8192, Some("test_e2e_stream"));
    let torrent = create_torrent(
        files.path(),
        CreateTorrentOptions {
            name: None,
            piece_length: Some(1024),
        },
    )
    .await?;

    let orig_content = std::fs::read(files.path().join("0.data")).unwrap();

    let server_session = Session::new_with_opts(
        "/does-not-matter".into(),
        crate::SessionOptions {
            disable_dht: true,
            persistence: false,
            listen_port_range: Some(16001..16100),
            enable_upnp_port_forwarding: false,
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

    let peer = SocketAddr::new(
        "127.0.0.1".parse().unwrap(),
        server_session.tcp_listen_port().unwrap(),
    );

    let client_session = Session::new_with_opts(
        "/does-not-matter".into(),
        crate::SessionOptions {
            disable_dht: true,
            persistence: false,
            listen_port_range: None,
            enable_upnp_port_forwarding: false,
            ..Default::default()
        },
    )
    .await?;

    info!("created client session");

    let client_handle = client_session
        .add_torrent(
            AddTorrent::from_bytes(torrent.as_bytes()?),
            Some(crate::AddTorrentOptions {
                paused: false,
                initial_peers: Some(vec![peer]),
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .unwrap();

    client_handle.wait_until_initialized().await?;

    info!("client torrent initialized, starting stream");

    let mut stream = client_handle.stream(0)?;
    let mut buf = Vec::<u8>::with_capacity(8192);
    stream.read_to_end(&mut buf).await?;

    if buf != orig_content {
        panic!("contents differ")
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_e2e_stream() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    timeout(Duration::from_secs(10), e2e_stream()).await?
}
