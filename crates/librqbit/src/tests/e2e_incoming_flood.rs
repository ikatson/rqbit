// End-to-end tests for the resource exhaustion reported in
// https://github.com/ikatson/rqbit/issues/525.
//
// These tests flood a session with incoming TCP connections the way a hot
// torrent does (thousands of peers, most of them useless) and assert that:
//
// - the peers map stays bounded (useless entries are evicted),
// - the session can still perform a real download while being flooded,
// - the listener task survives and keeps accepting and processing
//   connections,
// - the number of open file descriptors stays bounded.
//
// All flood connections complete a valid handshake against a live torrent,
// so the listener's handshake checks all succeed; failing checks and the
// pending-check cap are covered by the task_listener unit tests.
//
// They are heavier than the unit tests, but each finishes in well under a
// minute on a local machine.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use librqbit_core::hash_id::Id20;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    spawn,
    time::timeout,
};
use tracing::{error, error_span, info};

use crate::{
    AddTorrent, AddTorrentOptions, ConnectionOptions, PeerConnectionOptions, Session,
    SessionOptions, create_torrent,
    listen::{ListenerMode, ListenerOptions},
    spawn_utils::BlockingSpawner,
    tests::test_util::{create_default_random_dir_with_torrents, setup_test_logging},
    torrent_state::live::peers::MAX_TRACKED_PEERS,
};

const VICTIM_LISTEN_PORT: u16 = 29241;
const SEEDER_LISTEN_PORT: u16 = 29242;
// Only used by the test below, which is compiled only with the
// `disable-upload` feature.
#[cfg_attr(not(feature = "disable-upload"), allow(dead_code))]
const GATED_VICTIM_LISTEN_PORT: u16 = 29244;

// These tests measure process-wide resources (open FDs) and hammer localhost,
// so they are serialized against each other. They still run concurrently with
// the other tests, hence the generous FD bounds below.
static FLOOD_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// 1 byte pstr length + 19 bytes protocol + 8 reserved + 20 + 20.
const HANDSHAKE_LEN: usize = 68;

fn flood_peer_id() -> Id20 {
    Id20::new(*b"-qB4550-flood-test01")
}

/// Count open FDs of this process. Only works on Linux, returns None
/// elsewhere.
fn count_open_fds() -> Option<usize> {
    #[cfg(target_os = "linux")]
    return std::fs::read_dir("/proc/self/fd").ok().map(|d| d.count());
    #[cfg(not(target_os = "linux"))]
    return None;
}

enum FloodMode {
    /// Connect, complete the BitTorrent handshake, then close cleanly.
    /// Waiting for the server's handshake reply guarantees the peer was
    /// fully accepted (and thus tracked in the peers map) before we close
    /// the socket: the peer then dies and its entry becomes garbage.
    HandshakeThenDrop(Id20),
    /// Like [`FloodMode::HandshakeThenDrop`], but hold the connection open
    /// for a while before closing: exercises accepting many concurrent
    /// connections.
    HandshakeThenHold(Id20, Duration),
}

fn make_handshake_bytes(info_hash: Id20) -> [u8; HANDSHAKE_LEN] {
    let mut buf = [0u8; HANDSHAKE_LEN];
    // The buffer is exactly the size of a handshake.
    let _ = peer_binary_protocol::Handshake::new(info_hash, flood_peer_id())
        .serialize_unchecked_len(&mut buf);
    buf
}

async fn flood_once(addr: SocketAddr, mode: &FloodMode) {
    let (info_hash, hold) = match mode {
        FloodMode::HandshakeThenDrop(info_hash) => (*info_hash, None),
        FloodMode::HandshakeThenHold(info_hash, hold) => (*info_hash, Some(*hold)),
    };
    let hs = make_handshake_bytes(info_hash);

    // Transient connect/write failures (e.g. the accept backlog filling up)
    // are not interesting: the assertions at the end of the tests are the
    // real checks.
    let Ok(mut stream) = TcpStream::connect(addr).await else {
        return;
    };
    if stream.write_all(&hs).await.is_err() {
        return;
    }

    // Wait for the server's handshake reply: at this point the connection
    // was validated and the peer accepted. Then close the connection: the
    // peer dies and its map entry becomes garbage that the peers-map
    // pruning has to clean up.
    let mut reply = [0u8; HANDSHAKE_LEN];
    match timeout(Duration::from_secs(2), stream.read_exact(&mut reply)).await {
        Ok(Ok(_)) => {}
        // Server closed first (e.g. concurrency limit reached): fine.
        Ok(Err(_)) => return,
        Err(_) => return,
    }
    if let Some(hold) = hold {
        tokio::time::sleep(hold).await;
    }
    let _ = stream.shutdown().await;
}

async fn flood_worker(
    addr: SocketAddr,
    mode: FloodMode,
    mut iters: usize,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    while iters > 0 {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        iters -= 1;
        flood_once(addr, &mode).await;
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    Ok(())
}

/// A wave of connections that complete the handshake and then hold the
/// connection open for a bit: exercises accepting many concurrent
/// connections while they linger.
async fn hold_open_wave(addr: SocketAddr, info_hash: Id20, size: usize, hold: Duration) {
    let mut tasks = Vec::with_capacity(size);
    for _ in 0..size {
        let mode = FloodMode::HandshakeThenHold(info_hash, hold);
        tasks.push(spawn(async move { flood_once(addr, &mode).await }));
    }
    for t in tasks {
        let _ = t.await;
    }
}

/// Sends one valid handshake and waits for the session to close the
/// connection. Fails if the session leaves us hanging, which is what happens
/// when the listener task is stuck or dead.
async fn assert_listener_processes_connections(addr: SocketAddr, info_hash: Id20) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let hs = make_handshake_bytes(info_hash);
    stream.write_all(&hs).await.unwrap();
    // The session closes the connection (rejected because the torrent is
    // finished and upload is disabled, or after the peer read/write timeout).
    // Either way it must not strand us; a read error (e.g. ECONNRESET on
    // hard close) also proves the session dealt with the connection.
    let mut sink = Vec::new();
    let _ = timeout(Duration::from_secs(10), stream.read_to_end(&mut sink))
        .await
        .expect("listener should still process incoming connections");
}

async fn wait_until_finished(handle: &crate::torrent_state::ManagedTorrentHandle, secs: u64) {
    timeout(Duration::from_secs(secs), handle.wait_until_completed())
        .await
        .expect("torrent should complete in time")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_e2e_incoming_flood_bounded() {
    let _guard = FLOOD_TEST_LOCK.lock().await;
    let e2e_timeout = std::env::var("E2E_TIMEOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    setup_test_logging();
    match crate::try_increase_nofile_limit() {
        Ok(limit) => info!(limit, "increased ulimit"),
        Err(e) => error!(error=?e, "error increasing ulimit"),
    };

    timeout(Duration::from_secs(e2e_timeout), async {
        // 1. Payload torrent.
        let data_dir = create_default_random_dir_with_torrents(2, 512 * 1000, Some("rqbit_flood"));
        let torrent = create_torrent(
            data_dir.path(),
            crate::CreateTorrentOptions::default(),
            &BlockingSpawner::new(1),
        )
        .await
        .unwrap();
        let info_hash = torrent.info_hash();
        let torrent_bytes = torrent.as_bytes().unwrap();

        // 2. Seeder session holding the data.
        let seeder = Session::new_with_opts(
            std::env::temp_dir().join("rqbit_flood_seeder"),
            SessionOptions {
                dht: None,
                disable_local_service_discovery: true,
                listen: Some(ListenerOptions {
                    mode: ListenerMode::TcpOnly,
                    listen_addr: SocketAddr::new(
                        IpAddr::V4(Ipv4Addr::LOCALHOST),
                        SEEDER_LISTEN_PORT,
                    ),
                    ..Default::default()
                }),
                root_span: Some(error_span!(parent: None, "seeder")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let seeder_handle = seeder
            .add_torrent(
                AddTorrent::TorrentFileBytes(torrent_bytes.clone()),
                Some(AddTorrentOptions {
                    overwrite: true,
                    output_folder: Some(data_dir.path().to_str().unwrap().to_owned()),
                    ..Default::default()
                }),
            )
            .await
            .unwrap()
            .into_handle()
            .unwrap();
        wait_until_finished(&seeder_handle, 30).await;
        info!("seeder is ready");

        // 3. Victim session: low peer limit and aggressive read/write timeout
        //    so dead peers release their slots quickly. The pending handshake
        //    check cap is raised well above the flood's concurrency so this
        //    test exercises the peers map, not the cap.
        let outdir = tempfile::TempDir::with_prefix("rqbit_flood_victim").unwrap();
        let victim = Session::new_with_opts(
            outdir.path().join("out"),
            SessionOptions {
                dht: None,
                disable_local_service_discovery: true,
                listen: Some(ListenerOptions {
                    mode: ListenerMode::TcpOnly,
                    listen_addr: SocketAddr::new(
                        IpAddr::V4(Ipv4Addr::LOCALHOST),
                        VICTIM_LISTEN_PORT,
                    ),
                    max_pending_incoming_handshake_checks: 1024,
                    ..Default::default()
                }),
                connect: Some(ConnectionOptions {
                    peer_opts: Some(PeerConnectionOptions {
                        read_write_timeout: Some(Duration::from_millis(200)),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                peer_limit: Some(32),
                root_span: Some(error_span!(parent: None, "victim")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let victim_addr = victim
            .listen_addr()
            .expect("victim session should have a listen address");

        // Baseline FDs before the flood.
        let fd_baseline = count_open_fds();

        // 4. Add the torrent and wait until it is live, so every flood
        //    connection targets a live torrent.
        let handle = victim
            .add_torrent(
                AddTorrent::TorrentFileBytes(torrent_bytes),
                Some(AddTorrentOptions {
                    overwrite: true,
                    initial_peers: Some(vec![SocketAddr::new(
                        IpAddr::V4(Ipv4Addr::LOCALHOST),
                        SEEDER_LISTEN_PORT,
                    )]),
                    ..Default::default()
                }),
            )
            .await
            .unwrap()
            .into_handle()
            .unwrap();
        timeout(Duration::from_secs(30), async {
            while handle.stats().live.is_none() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("torrent should go live");
        info!("torrent is live, starting the flood");

        // 5. Flood the session while it downloads the torrent from the
        //    seeder, and keep flooding for a bit after it completes.
        let stop = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::new();
        for _ in 0..6 {
            workers.push(spawn(flood_worker(
                victim_addr,
                FloodMode::HandshakeThenDrop(info_hash),
                1200,
                stop.clone(),
            )));
        }
        for _ in 0..2 {
            workers.push(spawn(flood_worker(
                victim_addr,
                FloodMode::HandshakeThenDrop(info_hash),
                600,
                stop.clone(),
            )));
        }

        timeout(Duration::from_secs(120), handle.wait_until_completed())
            .await
            .expect("download should complete while the session is flooded")
            .unwrap();
        info!("download completed while flooded");

        // Give the flood a chance to hit the now-finished torrent too.
        tokio::time::sleep(Duration::from_secs(2)).await;

        // 6. Waves of lingering connections: many concurrent connections
        //    that are accepted, complete the handshake and stay open for a
        //    moment.
        hold_open_wave(victim_addr, info_hash, 128, Duration::from_millis(250)).await;
        hold_open_wave(victim_addr, info_hash, 300, Duration::from_millis(250)).await;

        // 7. Stop the flood.
        stop.store(true, Ordering::Relaxed);
        for w in workers {
            timeout(Duration::from_secs(30), w)
                .await
                .expect("flood worker should stop")
                .unwrap()
                .unwrap();
        }

        // 8. Boundedness: the peers map must not have grown beyond the
        //    configured bound, and the flood must have been processed.
        let stats = handle.stats();
        let peer_stats = &stats.live.as_ref().expect("live").snapshot.peer_stats;
        let tracked = peer_stats.queued
            + peer_stats.connecting
            + peer_stats.live
            + peer_stats.dead
            + peer_stats.not_needed;
        info!(?peer_stats, tracked, "peer stats after flood");
        assert!(
            tracked as usize <= MAX_TRACKED_PEERS + 8,
            "peers map should stay bounded, but tracks {tracked} peers"
        );
        assert!(
            peer_stats.seen > 200,
            "expected the flood to actually track peers, seen={}",
            peer_stats.seen
        );

        // 9. The listener must still be alive and processing connections
        //    after all that.
        assert_listener_processes_connections(victim_addr, info_hash).await;
        info!("listener still alive after flood");

        // 10. FDs must not have piled up.
        tokio::time::sleep(Duration::from_secs(2)).await;
        if let (Some(baseline), Some(after)) = (fd_baseline, count_open_fds()) {
            let growth = after.saturating_sub(baseline);
            // The bound is generous: the process may concurrently run other
            // tests. A real leak (e.g. sockets of pruned peers never closed)
            // would grow by thousands.
            assert!(
                growth < 2000,
                "fd count should stay bounded: baseline={baseline}, now={after}"
            );
        }

        info!("all flood assertions passed");
    })
    .await
    .expect("test_e2e_incoming_flood_bounded timed out");
}

/// The exact scenario from the issue: a fully downloaded torrent in a session
/// with upload disabled must not accept incoming peers at all, no matter how
/// many connect.
#[tokio::test(flavor = "multi_thread")]
#[cfg(feature = "disable-upload")]
async fn test_e2e_incoming_flood_finished_no_upload() {
    let _guard = FLOOD_TEST_LOCK.lock().await;
    setup_test_logging();
    match crate::try_increase_nofile_limit() {
        Ok(limit) => info!(limit, "increased ulimit"),
        Err(e) => error!(error=?e, "error increasing ulimit"),
    };

    timeout(Duration::from_secs(120), async {
        let data_dir =
            create_default_random_dir_with_torrents(1, 256 * 1000, Some("rqbit_flood_gate"));
        let torrent = create_torrent(
            data_dir.path(),
            crate::CreateTorrentOptions::default(),
            &BlockingSpawner::new(1),
        )
        .await
        .unwrap();
        let info_hash = torrent.info_hash();

        let outdir = tempfile::TempDir::with_prefix("rqbit_flood_gate_victim").unwrap();
        let session = Session::new_with_opts(
            outdir.path().join("out"),
            SessionOptions {
                dht: None,
                disable_local_service_discovery: true,
                disable_upload: true,
                listen: Some(ListenerOptions {
                    mode: ListenerMode::TcpOnly,
                    listen_addr: SocketAddr::new(
                        IpAddr::V4(Ipv4Addr::LOCALHOST),
                        GATED_VICTIM_LISTEN_PORT,
                    ),
                    max_pending_incoming_handshake_checks: 1024,
                    ..Default::default()
                }),
                connect: Some(ConnectionOptions {
                    peer_opts: Some(PeerConnectionOptions {
                        read_write_timeout: Some(Duration::from_millis(200)),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                peer_limit: Some(16),
                root_span: Some(error_span!(parent: None, "gate_victim")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let listen_addr = session
            .listen_addr()
            .expect("session should have a listen address");

        let handle = session
            .add_torrent(
                AddTorrent::TorrentFileBytes(torrent.as_bytes().unwrap()),
                Some(AddTorrentOptions {
                    overwrite: true,
                    output_folder: Some(data_dir.path().to_str().unwrap().to_owned()),
                    ..Default::default()
                }),
            )
            .await
            .unwrap()
            .into_handle()
            .unwrap();
        wait_until_finished(&handle, 30).await;
        info!("torrent is finished and upload is disabled");

        let fd_baseline = count_open_fds();

        let stop = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::new();
        for _ in 0..5 {
            workers.push(spawn(flood_worker(
                listen_addr,
                FloodMode::HandshakeThenDrop(info_hash),
                800,
                stop.clone(),
            )));
        }

        tokio::time::sleep(Duration::from_secs(3)).await;
        stop.store(true, Ordering::Relaxed);
        for w in workers {
            timeout(Duration::from_secs(30), w)
                .await
                .expect("flood worker should stop")
                .unwrap()
                .unwrap();
        }

        // The gate must have prevented any peer from being tracked.
        let stats = handle.stats();
        let peer_stats = &stats.live.as_ref().expect("live").snapshot.peer_stats;
        info!(?peer_stats, "peer stats after gated flood");
        assert!(
            peer_stats.seen < 50,
            "finished torrent with upload disabled should not track incoming peers, seen={}",
            peer_stats.seen
        );

        // The listener must still be alive and processing connections.
        assert_listener_processes_connections(listen_addr, info_hash).await;

        if let (Some(baseline), Some(after)) = (fd_baseline, count_open_fds()) {
            let growth = after.saturating_sub(baseline);
            // Generous for the same reason as in the other test above.
            assert!(
                growth < 2000,
                "fd count should stay bounded: baseline={baseline}, now={after}"
            );
        }

        info!("all gated flood assertions passed");
    })
    .await
    .unwrap();
}
