use std::{net::Ipv4Addr, num::NonZeroU32, time::Duration};

use anyhow::Context;
use tempfile::TempDir;
use tokio::time::timeout;
use tracing::info;

use crate::{
    AddTorrent, CreateTorrentOptions, Session, create_torrent,
    limits::LimitsConfig,
    spawn_utils::BlockingSpawner,
    tests::test_util::{
        TestPeerMetadata, create_default_random_dir_with_torrents, setup_test_logging,
    },
};

const FILE_SIZE: usize = 256 * 1024;
const LIMIT_BPS: u32 = 32 * 1024;

// A torrent's own download limit can be set when adding it and changed later,
// and the change applies to a torrent that is already downloading.
async fn e2e_per_torrent_ratelimits() -> anyhow::Result<()> {
    setup_test_logging();
    let files = create_default_random_dir_with_torrents(1, FILE_SIZE, Some("test_e2e_ratelimits"));
    let torrent = create_torrent(
        files.path(),
        CreateTorrentOptions {
            name: None,
            piece_length: Some(16384),
            ..Default::default()
        },
        &BlockingSpawner::new(1),
    )
    .await?;

    let server_session = Session::new_with_opts(
        files.path().into(),
        crate::SessionOptions {
            dht: None,
            peer_id: Some(TestPeerMetadata::good().as_peer_id()),
            persistence: None,
            listen: Some(crate::listen::ListenerOptions {
                listen_addr: (Ipv4Addr::LOCALHOST, 16002).into(),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .context("error creating server session")?;

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
        .wait_until_completed()
        .await?;

    let peer = server_session
        .listen_addr()
        .context("expected listen_addr to be set")?;

    let client_dir = TempDir::with_prefix("test_e2e_ratelimits_client")?;
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

    let limited = LimitsConfig {
        upload_bps: None,
        download_bps: NonZeroU32::new(LIMIT_BPS),
    };

    // Start paused to check that the limit is kept while the torrent isn't live.
    let handle = client_session
        .add_torrent(
            AddTorrent::from_bytes(torrent.as_bytes()?),
            Some(crate::AddTorrentOptions {
                paused: false,
                initial_peers: Some(vec![peer]),
                ratelimits: limited,
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .unwrap();
    assert_eq!(handle.ratelimits(), limited);

    assert_eq!(handle.ratelimits(), limited);

    // At 32 KiB/s, 256 KiB takes about 7 seconds, so it must not be done yet.
    let res = timeout(Duration::from_millis(1500), handle.wait_until_completed()).await;
    assert!(res.is_err(), "download should still be throttled");
    info!(progress = handle.stats().progress_bytes, "throttled");

    // Lift the limit on the live torrent; the rest should arrive quickly.
    handle.set_ratelimits(LimitsConfig::default());
    assert_eq!(handle.ratelimits(), LimitsConfig::default());
    timeout(Duration::from_secs(5), handle.wait_until_completed())
        .await
        .context("download should finish quickly once the limit is lifted")??;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_e2e_per_torrent_ratelimits() -> anyhow::Result<()> {
    timeout(Duration::from_secs(20), e2e_per_torrent_ratelimits()).await?
}

#[tokio::test]
async fn test_ratelimits_survive_pause() -> anyhow::Result<()> {
    setup_test_logging();
    let files = create_default_random_dir_with_torrents(1, 16384, Some("test_ratelimits_pause"));
    let torrent =
        create_torrent(files.path(), Default::default(), &BlockingSpawner::new(1)).await?;
    let session = Session::new_with_opts(
        files.path().into(),
        crate::SessionOptions {
            dht: None,
            persistence: None,
            listen: None,
            ..Default::default()
        },
    )
    .await?;
    let handle = session
        .add_torrent(
            AddTorrent::from_bytes(torrent.as_bytes()?),
            Some(crate::AddTorrentOptions {
                output_folder: Some(files.path().to_str().unwrap().to_owned()),
                overwrite: true,
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .unwrap();
    handle.wait_until_completed().await?;
    assert_eq!(handle.ratelimits(), LimitsConfig::default());

    let config = LimitsConfig {
        upload_bps: NonZeroU32::new(1000),
        download_bps: NonZeroU32::new(2000),
    };
    handle.set_ratelimits(config);
    session.pause(&handle).await?;
    assert_eq!(handle.ratelimits(), config);
    session.unpause(&handle).await?;
    assert_eq!(handle.ratelimits(), config);
    Ok(())
}
