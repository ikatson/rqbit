use std::{
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

use anyhow::Context;
use librqbit_core::{Id20, magnet::Magnet};
use tempfile::TempDir;
use tokio::time::{sleep, timeout};
use tracing::info;

use crate::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, CreateTorrentOptions, Session,
    SessionOptions, create_torrent,
    listen::ListenerOptions,
    spawn_utils::BlockingSpawner,
    tests::test_util::{
        TestPeerMetadata, create_default_random_dir_with_torrents, setup_test_logging,
    },
};

fn offline_session_options() -> SessionOptions {
    SessionOptions {
        dht: None,
        persistence: None,
        disable_trackers: true,
        disable_local_service_discovery: true,
        peer_id: Some(TestPeerMetadata::good().as_peer_id()),
        ..Default::default()
    }
}

// AddTorrentOptions.peer_feed resolves metadata for a magnet that has no DHT,
// no trackers and no initial peers: the peer only shows up 1s into the add and
// only through the feed, so resolution has to keep waiting for it. Without a
// feed that same add fails immediately with "no known way to resolve peers".
#[tokio::test(flavor = "multi_thread")]
async fn peer_feed_resolves_magnet_metadata() -> anyhow::Result<()> {
    setup_test_logging();
    let files = create_default_random_dir_with_torrents(1, 8192, Some("rqbit_p3_peer_feed"));
    let torrent = create_torrent(
        files.path(),
        CreateTorrentOptions {
            piece_length: Some(1024),
            ..Default::default()
        },
        &BlockingSpawner::new(1),
    )
    .await?;
    let info_hash = torrent.info_hash();
    let seeder = Session::new_with_opts(
        files.path().into(),
        SessionOptions {
            listen: Some(ListenerOptions {
                listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
                ..Default::default()
            }),
            ..offline_session_options()
        },
    )
    .await?;
    timeout(
        Duration::from_secs(5),
        seeder
            .add_torrent(
                AddTorrent::from_bytes(torrent.as_bytes()?),
                Some(AddTorrentOptions {
                    overwrite: true,
                    output_folder: Some(files.path().to_str().unwrap().to_owned()),
                    ..Default::default()
                }),
            )
            .await?
            .into_handle()
            .unwrap()
            .wait_until_completed(),
    )
    .await?
    .context("error seeding torrent")?;
    let peer: SocketAddr = seeder.listen_addr().context("no listen_addr")?;
    info!(?peer, "seeder ready");

    let client_dir = TempDir::with_prefix("rqbit_p3_client")?;
    let client =
        Session::new_with_opts(client_dir.path().into(), offline_session_options()).await?;
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tokio::spawn(async move {
        sleep(Duration::from_secs(1)).await;
        let _ = tx.send(peer).await;
    });

    let magnet = Magnet::from_id20(info_hash, Vec::new(), None).to_string();
    let response = timeout(
        Duration::from_secs(20),
        client.add_torrent(
            AddTorrent::Url(magnet.into()),
            Some(AddTorrentOptions {
                list_only: true,
                peer_feed: Some(rx),
                ..Default::default()
            }),
        ),
    )
    .await?
    .context("add_torrent timed out - the feed peer was never used")?;

    let AddTorrentResponse::ListOnly(r) = response else {
        return Err(anyhow::anyhow!("expected a ListOnly response"));
    };
    assert_eq!(r.seen_peers, vec![peer]);
    assert!(!r.torrent_bytes.is_empty());

    // Without a feed the very same add has nothing to wait for and fails.
    let magnet = Magnet::from_id20(Id20::default(), Vec::new(), None).to_string();
    let err = client
        .add_torrent(
            AddTorrent::Url(magnet.into()),
            Some(AddTorrentOptions {
                list_only: true,
                ..Default::default()
            }),
        )
        .await
        .err()
        .expect("expected the add to fail");
    assert!(
        format!("{err:#}").contains("no known way to resolve peers"),
        "unexpected error: {err:#}"
    );
    Ok(())
}
