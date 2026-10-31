use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Context;
use tempfile::TempDir;
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
    time::timeout,
};

use crate::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, Session, SessionOptions, create_torrent,
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

/// A seeder plus a TCP proxy in front of it that kills the first
/// connection right after reading the handshake (a transient peer error),
/// and forwards every later connection. Returns the proxy address.
async fn flaky_seeder() -> anyhow::Result<(
    Session2,
    SocketAddr,
    Arc<AtomicU32>,
    librqbit_core::Id20,
    Vec<u8>,
)> {
    let files = create_default_random_dir_with_torrents(1, 256 * 1024, Some("rqbit_p9_peer_retry"));
    let torrent = create_torrent(
        files.path(),
        CreateTorrentOptions {
            piece_length: Some(16384),
            ..Default::default()
        },
        &BlockingSpawner::new(1),
    )
    .await?;
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
        Duration::from_secs(10),
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
    let upstream: SocketAddr = seeder.listen_addr().context("no listen_addr")?;

    let proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let proxy_addr = proxy.local_addr()?;
    let accepted = Arc::new(AtomicU32::new(0));
    let counter = accepted.clone();
    tokio::spawn(async move {
        while let Ok((mut down, _)) = proxy.accept().await {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                if n == 0 {
                    // Read the 68-byte BitTorrent handshake, then hang up.
                    let mut buf = [0u8; 68];
                    let _ = down.read_exact(&mut buf).await;
                    return;
                }
                if let Ok(mut up) = TcpStream::connect(upstream).await {
                    let _ = tokio::io::copy_bidirectional(&mut down, &mut up).await;
                }
            });
        }
    });
    Ok((
        Session2 {
            _seeder: seeder,
            _files: files,
        },
        proxy_addr,
        accepted,
        torrent.info_hash(),
        torrent.as_bytes()?.to_vec(),
    ))
}

struct Session2 {
    _seeder: Arc<Session>,
    _files: TempDir,
}

// P9: one transient peer error used to cost 10-20 s (backoff min 10 s,
// factor 6, jitter) before librqbit reconnected. The only peer here drops
// the first connection; the download must still finish within a few seconds.
#[tokio::test(flavor = "multi_thread")]
async fn transient_peer_error_is_retried_quickly() -> anyhow::Result<()> {
    setup_test_logging();
    let (_seeder, proxy, accepted, _ih, torrent_bytes) = flaky_seeder().await?;
    let client_dir = TempDir::with_prefix("rqbit_p9_client")?;
    let client =
        Session::new_with_opts(client_dir.path().into(), offline_session_options()).await?;
    let start = Instant::now();
    let handle = client
        .add_torrent(
            AddTorrent::from_bytes(torrent_bytes),
            Some(AddTorrentOptions {
                initial_peers: Some(vec![proxy]),
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .context("handle")?;
    timeout(Duration::from_secs(8), handle.wait_until_completed())
        .await
        .context("download did not finish: the peer was not retried quickly enough")??;
    let elapsed = start.elapsed();
    assert!(
        accepted.load(Ordering::SeqCst) >= 2,
        "the peer was never retried"
    );
    assert!(elapsed < Duration::from_secs(8), "{elapsed:?}");
    Ok(())
}

// The metadata-resolution half of P9 (a failed peer is dialed again when the
// address stream yields it a second time) has its end-to-end regression test in
// the downstream branch, where `AddTorrentOptions::peer_feed` provides a
// re-yielding peer source. Upstream, the equivalent test needs DHT or a tracker
// that re-announces, so the unit test
// `dht_utils::tests::test_should_dial_retries_failed_peers_after_delay` covers
// the new policy here.
