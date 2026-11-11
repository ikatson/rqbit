//! Sessions with I2P, against the fake SAM bridge of `crate::i2p::fake_bridge` on loopback.
//!
//! The I2P tracker of these tests is a fake HTTP tracker on loopback that the bridge hosts as
//! the service `tracker.i2p`: it hands every announcer the destinations of the others (and any
//! the test adds), the way an I2P tracker does. Peers inside I2P are real sessions talking to
//! each other through the bridge, or wire-level peers the bridge hosts as services.

use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::NonZeroU32,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::SeqCst},
    },
    time::{Duration, Instant},
};

use anyhow::Context;
use buffers::ByteBuf;
use bytes::Bytes;
use librqbit_core::{
    Id20,
    i2p::{I2pDestination, is_synthetic, is_synthetic_ip, synthetic_addr},
};
use parking_lot::Mutex;
use peer_binary_protocol::extended::{handshake::ExtendedHandshake, ut_pex::UtPex};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::timeout,
};
use tokio_util::sync::CancellationToken;
use tracker_comms::{I2P_CLEARNET_SKIPPED, I2P_NOT_ENABLED};

use crate::{
    AddTorrent, AddTorrentOptions, ConnectionOptions, CreateTorrentOptions, I2pOptions,
    ListenerOptions, PeerConnectionOptions, Session, SessionOptions, create_torrent,
    i2p::{fake_bridge::FakeSamBridge, sam::Reply},
    limits::LimitsConfig,
    spawn_utils::BlockingSpawner,
    stream_connect::{ConnectionKind, StreamConnector, StreamConnectorArgs},
    tests::{
        e2e_trackers::{Announce, FakeTracker, percent_decode},
        fake_peer::{handshake_bytes, peer_id, run},
        test_util::create_default_random_dir_with_torrents,
    },
    torrent_state::{
        ManagedTorrentHandle,
        live::peer::stats::snapshot::{PeerStatsFilter, PeerStatsFilterState},
    },
};

const WAIT: Duration = Duration::from_secs(50);
const PIECE_LEN: u32 = 32 * 1024;
const TRACKER_NAME: &str = "tracker.i2p";
/// Long enough that no session gives up on a silent connection during a test.
const PATIENT: Duration = Duration::from_secs(120);

async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) -> anyhow::Result<()> {
    let deadline = Instant::now() + WAIT;
    while !cond() {
        anyhow::ensure!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The I2P tracker
// ---------------------------------------------------------------------------------------------

/// An HTTP tracker reachable inside I2P as `tracker.i2p`. It records every announce and
/// answers with the destinations (`ip=` of the announce) of every other announcer of the same
/// torrent, plus the ones added with [`I2pTracker::add_peer`], in the dictionary form.
struct I2pTracker {
    dest: I2pDestination,
    announces: Arc<Mutex<Vec<Announce>>>,
    extra: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl Drop for I2pTracker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl I2pTracker {
    async fn start(bridge: &FakeSamBridge) -> anyhow::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let dest = bridge.add_service(Some(TRACKER_NAME), listener.local_addr()?);
        let announces: Arc<Mutex<Vec<Announce>>> = Default::default();
        let extra: Arc<Mutex<Vec<String>>> = Default::default();
        let (a, e) = (announces.clone(), extra.clone());
        let task = tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                tokio::spawn(serve_i2p_announce(sock, a.clone(), e.clone()));
            }
        });
        Ok(Self {
            dest,
            announces,
            extra,
            task,
        })
    }

    fn url(&self) -> String {
        format!("http://{TRACKER_NAME}/announce")
    }

    /// Hands out `dest` to every announcer from now on.
    fn add_peer(&self, dest: &I2pDestination) {
        self.extra
            .lock()
            .push(format!("{}.i2p", dest.to_i2p_base64()));
    }

    fn announces(&self) -> Vec<Announce> {
        self.announces.lock().clone()
    }

    /// The destinations (`ip=`) every announce so far named.
    fn announcers(&self) -> HashSet<String> {
        self.announces()
            .iter()
            .filter_map(|a| a.param("ip"))
            .map(|ip| String::from_utf8_lossy(ip).into_owned())
            .collect()
    }
}

async fn serve_i2p_announce(
    mut sock: TcpStream,
    announces: Arc<Mutex<Vec<Announce>>>,
    extra: Arc<Mutex<Vec<String>>>,
) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        match sock.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let head = String::from_utf8_lossy(&buf).into_owned();
    let target = head
        .lines()
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .unwrap_or_default();
    let (_, query) = target.split_once('?').unwrap_or((target, ""));
    let announce = Announce {
        params: query
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_owned(), percent_decode(v)))
            .collect(),
    };
    let peers: Vec<String> = {
        let mut g = announces.lock();
        let mut peers: Vec<String> = Vec::new();
        for a in g.iter() {
            if a.param("info_hash") == announce.param("info_hash")
                && a.param("ip") != announce.param("ip")
                && let Some(ip) = a.param("ip")
            {
                let ip = String::from_utf8_lossy(ip).into_owned();
                if !peers.contains(&ip) {
                    peers.push(ip);
                }
            }
        }
        peers.extend(extra.lock().iter().cloned());
        g.push(announce);
        peers
    };
    let mut body = b"d8:intervali1800e5:peersl".to_vec();
    for ip in peers {
        body.extend_from_slice(format!("d2:ip{}:{ip}4:porti6881ee", ip.len()).as_bytes());
    }
    body.extend_from_slice(b"ee");
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = sock.write_all(head.as_bytes()).await;
    let _ = sock.write_all(&body).await;
    let _ = sock.shutdown().await;
}

/// Whether a clearnet tracker request tells anything about I2P: a synthetic address or a
/// destination.
fn leaks_i2p(a: &Announce) -> bool {
    a.params.iter().any(|(_, v)| {
        let v = String::from_utf8_lossy(v);
        v.ends_with(".i2p")
            || v.parse::<IpAddr>().is_ok_and(|ip| is_synthetic_ip(&ip))
            || v.parse::<SocketAddr>().is_ok_and(|a| is_synthetic(&a))
    })
}

// ---------------------------------------------------------------------------------------------
// The bridge's command log
// ---------------------------------------------------------------------------------------------

fn parsed_commands(bridge: &FakeSamBridge) -> Vec<Reply> {
    bridge
        .commands()
        .iter()
        .filter_map(|l| Reply::parse(l).ok())
        .collect()
}

fn stream_accepts_of(bridge: &FakeSamBridge, sid: &str) -> usize {
    parsed_commands(bridge)
        .iter()
        .filter(|c| (c.topic.as_str(), c.kind.as_str()) == ("STREAM", "ACCEPT"))
        .filter(|c| c.get("ID") == Some(sid))
        .count()
}

fn assert_no_violations(bridge: &FakeSamBridge) {
    assert_eq!(bridge.violations(), Vec::<String>::new(), "SAM violations");
}

// ---------------------------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------------------------

/// A one-file torrent with random content, without trackers.
struct Content {
    dir: TempDir,
    torrent: Bytes,
    info_hash: Id20,
    data: Vec<u8>,
    pieces: u32,
}

async fn content(pieces: u32) -> anyhow::Result<Content> {
    let dir = create_default_random_dir_with_torrents(
        1,
        (PIECE_LEN * pieces) as usize,
        Some("rqbit_i2p_content"),
    );
    let torrent = create_torrent(
        dir.path(),
        CreateTorrentOptions {
            name: None,
            piece_length: Some(PIECE_LEN),
            ..Default::default()
        },
        &BlockingSpawner::new(1),
    )
    .await?;
    let data = std::fs::read(dir.path().join("0.data"))?;
    Ok(Content {
        torrent: torrent.as_bytes()?,
        info_hash: torrent.info_hash(),
        dir,
        data,
        pieces,
    })
}

#[derive(Default)]
struct NodeOpts {
    i2p: Option<I2pOptions>,
    /// Seed the content from its own folder instead of downloading it.
    seed: bool,
    /// Listen for clearnet peers on loopback.
    listen: bool,
    trackers: Vec<String>,
    initial_peers: Vec<SocketAddr>,
    upload_bps: Option<u32>,
    peer_limit: Option<usize>,
    force_tracker_interval: Option<Duration>,
}

struct Node {
    session: Arc<Session>,
    handle: ManagedTorrentHandle,
    out: PathBuf,
    _dir: TempDir,
}

impl Node {
    async fn start(c: &Content, opts: NodeOpts) -> anyhow::Result<Self> {
        let dir = TempDir::with_prefix("rqbit_i2p_node")?;
        let torrent_opts = opts_torrent(&opts);
        let session = Session::new_with_opts(
            dir.path().into(),
            SessionOptions {
                dht: None,
                persistence: None,
                listen: opts.listen.then(|| ListenerOptions {
                    listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
                    ipv4_only: true,
                    ..Default::default()
                }),
                connect: Some(ConnectionOptions::default()),
                ipv4_only: true,
                disable_local_service_discovery: true,
                ratelimits: LimitsConfig {
                    upload_bps: opts.upload_bps.and_then(NonZeroU32::new),
                    download_bps: None,
                },
                peer_limit: opts.peer_limit,
                i2p: opts.i2p,
                ..Default::default()
            },
        )
        .await
        .context("error creating session")?;
        let handle = Self::add(&session, c, &dir, &torrent_opts).await?;
        let out = if opts.seed {
            c.dir.path().to_owned()
        } else {
            dir.path().join("out")
        };
        if opts.seed {
            timeout(WAIT, handle.wait_until_completed())
                .await
                .context("the seeder did not become a seeder")??;
        }
        Ok(Self {
            session,
            handle,
            out,
            _dir: dir,
        })
    }

    async fn add(
        session: &Arc<Session>,
        c: &Content,
        dir: &TempDir,
        t: &TorrentOpts,
    ) -> anyhow::Result<ManagedTorrentHandle> {
        let out = if t.seed {
            c.dir.path().to_owned()
        } else {
            dir.path().join("out")
        };
        let handle = session
            .add_torrent(
                AddTorrent::from_bytes(c.torrent.clone()),
                Some(AddTorrentOptions {
                    paused: false,
                    output_folder: Some(out.to_str().context("path")?.to_owned()),
                    overwrite: true,
                    trackers: (!t.trackers.is_empty()).then(|| t.trackers.clone()),
                    initial_peers: (!t.initial_peers.is_empty()).then(|| t.initial_peers.clone()),
                    force_tracker_interval: t.force_tracker_interval,
                    peer_opts: Some(PeerConnectionOptions {
                        read_write_timeout: Some(PATIENT),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
            )
            .await?
            .into_handle()
            .context("into_handle")?;
        handle.wait_until_initialized().await?;
        Ok(handle)
    }

    fn addr(&self) -> SocketAddr {
        self.session
            .listen_addr()
            .expect("a node without a listener")
    }

    /// Waits for the download, then checks it against the content.
    async fn wait_complete(&self, c: &Content) -> anyhow::Result<()> {
        timeout(WAIT, self.handle.wait_until_completed())
            .await
            .context("the download did not complete")??;
        let got = std::fs::read(self.out.join("0.data"))?;
        anyhow::ensure!(got == c.data, "the downloaded file differs");
        Ok(())
    }
}

struct TorrentOpts {
    seed: bool,
    trackers: Vec<String>,
    initial_peers: Vec<SocketAddr>,
    force_tracker_interval: Option<Duration>,
}

fn opts_torrent(o: &NodeOpts) -> TorrentOpts {
    TorrentOpts {
        seed: o.seed,
        trackers: o.trackers.clone(),
        initial_peers: o.initial_peers.clone(),
        force_tracker_interval: o.force_tracker_interval,
    }
}

fn i2p(bridge: &FakeSamBridge, mixed_mode: bool) -> Option<I2pOptions> {
    Some(I2pOptions {
        sam_addr: bridge.addr(),
        mixed_mode,
        ..Default::default()
    })
}

/// Every peer `handle` knows, with its connection kind and state.
fn peers(handle: &ManagedTorrentHandle) -> Vec<(SocketAddr, Option<ConnectionKind>, &'static str)> {
    handle.live().map_or_else(Vec::new, |l| {
        l.per_peer_stats_snapshot(PeerStatsFilter {
            state: PeerStatsFilterState::All,
        })
        .peers
        .iter()
        .filter_map(|(k, p)| Some((k.parse().ok()?, p.conn_kind, p.state)))
        .collect()
    })
}

/// Starts a seeder of `c` inside I2P and waits until the tracker knows its destination.
/// Returns it with the id of its SAM session.
async fn i2p_seeder(
    bridge: &FakeSamBridge,
    tracker: &I2pTracker,
    c: &Content,
    opts: NodeOpts,
) -> anyhow::Result<(Node, String)> {
    let sessions_before = bridge.session_history().len();
    let seeder = Node::start(
        c,
        NodeOpts {
            i2p: i2p(bridge, false),
            seed: true,
            trackers: vec![tracker.url()],
            ..opts
        },
    )
    .await?;
    wait_for("the seeder's announce", || {
        bridge.session_history().len() > sessions_before && !tracker.announcers().is_empty()
    })
    .await?;
    let sid = bridge.session_history()[sessions_before].id.clone();
    Ok((seeder, sid))
}

// ---------------------------------------------------------------------------------------------
// A wire-level peer
// ---------------------------------------------------------------------------------------------

/// The payloads of the extended messages a [`WirePeer`] got: `(extended id, payload)`.
type ExtendedLog = Arc<Mutex<Vec<(u8, Vec<u8>)>>>;

/// A peer on a loopback listener that answers the handshake of whoever connects, sends an
/// extended handshake offering PEX (id 1) and a bitfield (all pieces or none), never unchokes,
/// and records the payload of every extended message it gets.
struct WirePeer {
    addr: SocketAddr,
    extended: ExtendedLog,
    task: JoinHandle<()>,
}

impl Drop for WirePeer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl WirePeer {
    async fn start(info_hash: Id20, pieces: u32, has_all: bool) -> anyhow::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = listener.local_addr()?;
        let extended: ExtendedLog = Default::default();
        let ext = extended.clone();
        let task = tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let ext = ext.clone();
                tokio::spawn(async move {
                    if let Err(e) = wire_peer_conn(sock, info_hash, pieces, has_all, ext).await {
                        tracing::debug!("wire peer connection ended: {e:#}");
                    }
                });
            }
        });
        Ok(Self {
            addr,
            extended,
            task,
        })
    }

    /// The extended handshake the peer got, bencoded.
    fn extended_handshake(&self) -> Option<Vec<u8>> {
        self.extended
            .lock()
            .iter()
            .find(|(id, _)| *id == 0)
            .map(|(_, p)| p.clone())
    }

    /// Every PEX message the peer got, bencoded.
    fn pex(&self) -> Vec<Vec<u8>> {
        self.extended
            .lock()
            .iter()
            .filter(|(id, _)| *id == 1)
            .map(|(_, p)| p.clone())
            .collect()
    }
}

async fn wire_peer_conn(
    mut sock: TcpStream,
    info_hash: Id20,
    pieces: u32,
    has_all: bool,
    extended: ExtendedLog,
) -> anyhow::Result<()> {
    let mut hs = [0u8; 68];
    sock.read_exact(&mut hs).await?;
    anyhow::ensure!(hs[28..48] == info_hash.0, "wrong info hash");
    sock.write_all(&handshake_bytes(info_hash, &peer_id(b"-WP0001-")))
        .await?;
    let frame = |id: u8, payload: &[u8]| {
        let mut f = Vec::with_capacity(5 + payload.len());
        let len = u32::try_from(payload.len() + 1).expect("a short test message");
        f.extend_from_slice(&len.to_be_bytes());
        f.push(id);
        f.extend_from_slice(payload);
        f
    };
    sock.write_all(&frame(20, b"\x00d1:md6:ut_pexi1eee"))
        .await?;
    let mut bits = vec![0u8; pieces.div_ceil(8) as usize];
    if has_all {
        for i in 0..pieces as usize {
            bits[i / 8] |= 0x80 >> (i % 8);
        }
    }
    sock.write_all(&frame(5, &bits)).await?;
    loop {
        let mut len = [0u8; 4];
        sock.read_exact(&mut len).await?;
        let len = u32::from_be_bytes(len) as usize;
        anyhow::ensure!(len <= 1 << 20, "frame too long");
        let mut msg = vec![0u8; len];
        sock.read_exact(&mut msg).await?;
        if let [20, ext_id, payload @ ..] = msg.as_slice() {
            extended.lock().push((*ext_id, payload.to_vec()));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------------

/// Two sessions find each other through an I2P tracker reached through the bridge; the
/// leecher dials the seeder over I2P and the seeder takes the stream with STREAM ACCEPT.
#[tokio::test(flavor = "multi_thread")]
async fn test_download_between_two_i2p_sessions() -> anyhow::Result<()> {
    run(async {
        let bridge = FakeSamBridge::start().await?;
        let tracker = I2pTracker::start(&bridge).await?;
        let c = content(4).await?;
        // The upload is throttled so that the connection is still up while the test looks.
        let (seeder, seeder_sid) = i2p_seeder(
            &bridge,
            &tracker,
            &c,
            NodeOpts {
                upload_bps: Some(64 * 1024),
                ..Default::default()
            },
        )
        .await?;
        let leecher = Node::start(
            &c,
            NodeOpts {
                i2p: i2p(&bridge, false),
                trackers: vec![tracker.url()],
                ..Default::default()
            },
        )
        .await?;
        wait_for("a live I2P peer in the leecher's stats", || {
            peers(&leecher.handle).iter().any(|(a, kind, state)| {
                is_synthetic(a) && *kind == Some(ConnectionKind::I2p) && *state == "live"
            })
        })
        .await?;
        leecher.wait_complete(&c).await?;

        let conns = leecher.session.stats_snapshot().connections;
        assert!(conns.i2p.successes > 0, "no I2P connection");
        assert_eq!(conns.tcp.v4.attempts + conns.tcp.v6.attempts, 0);

        // The seeder never dialed: the leecher came in through its STREAM ACCEPT.
        assert_eq!(seeder.session.stats_snapshot().connections.i2p.attempts, 0);
        let seeder_peers = peers(&seeder.handle);
        assert!(
            !seeder_peers.is_empty() && seeder_peers.iter().all(|(a, ..)| is_synthetic(a)),
            "{seeder_peers:?}"
        );
        assert!(stream_accepts_of(&bridge, &seeder_sid) > 0);
        assert!(bridge.service_connections(&tracker.dest) >= 2);
        assert_no_violations(&bridge);
        Ok(())
    })
    .await
}

/// With mixed mode off, an I2P torrent touches nothing in the clearnet: not its clearnet
/// tracker, not a clearnet peer it is given.
#[tokio::test(flavor = "multi_thread")]
async fn test_i2p_torrent_does_not_leak_into_the_clearnet() -> anyhow::Result<()> {
    run(async {
        let bridge = FakeSamBridge::start().await?;
        let tracker = I2pTracker::start(&bridge).await?;
        let clear_tracker = FakeTracker::start().await?;
        let c = content(4).await?;
        let (_seeder, _) = i2p_seeder(&bridge, &tracker, &c, NodeOpts::default()).await?;
        let clear_seeder = Node::start(
            &c,
            NodeOpts {
                seed: true,
                listen: true,
                ..Default::default()
            },
        )
        .await?;
        let leecher = Node::start(
            &c,
            NodeOpts {
                i2p: i2p(&bridge, false),
                trackers: vec![tracker.url(), clear_tracker.url()],
                initial_peers: vec![clear_seeder.addr()],
                ..Default::default()
            },
        )
        .await?;
        leecher.wait_complete(&c).await?;
        let clear_url = clear_tracker.url();
        wait_for("the error of the clearnet tracker", || {
            leecher.handle.trackers().iter().any(|t| {
                t.url == clear_url && t.last_error.as_deref() == Some(I2P_CLEARNET_SKIPPED)
            })
        })
        .await?;

        assert_eq!(clear_tracker.announces().len(), 0);
        assert_eq!(clear_tracker.sources(), Vec::<SocketAddr>::new());
        assert_eq!(
            peers(&clear_seeder.handle).len(),
            0,
            "the clearnet seeder was reached"
        );
        let conns = leecher.session.stats_snapshot().connections;
        assert_eq!(conns.tcp.v4.attempts + conns.tcp.v6.attempts, 0);
        assert_eq!(conns.utp.v4.attempts + conns.utp.v6.attempts, 0);
        assert!(peers(&leecher.handle).iter().all(|(a, ..)| is_synthetic(a)));
        assert_no_violations(&bridge);
        Ok(())
    })
    .await
}

/// With mixed mode on, an I2P torrent uses both networks, and nothing of I2P reaches the
/// clearnet: no synthetic address in a clearnet tracker request or in the PEX a clearnet peer
/// gets. An I2P peer is not offered PEX at all.
#[tokio::test(flavor = "multi_thread")]
async fn test_mixed_mode_keeps_i2p_addresses_out_of_the_clearnet() -> anyhow::Result<()> {
    run(async {
        let bridge = FakeSamBridge::start().await?;
        let tracker = I2pTracker::start(&bridge).await?;
        let clear_tracker = FakeTracker::start().await?;
        let c = content(4).await?;
        // An I2P peer with every piece that never unchokes: the connection stays up.
        let i2p_peer = WirePeer::start(c.info_hash, c.pieces, true).await?;
        tracker.add_peer(&bridge.add_service(None, i2p_peer.addr));
        let clear_peer = WirePeer::start(c.info_hash, c.pieces, false).await?;

        let node = Node::start(
            &c,
            NodeOpts {
                i2p: i2p(&bridge, true),
                trackers: vec![tracker.url(), clear_tracker.url()],
                initial_peers: vec![clear_peer.addr],
                ..Default::default()
            },
        )
        .await?;
        wait_for("both peers live", || {
            let p = peers(&node.handle);
            p.iter().any(|(a, _, s)| is_synthetic(a) && *s == "live")
                && p.iter()
                    .any(|(a, _, s)| *a == clear_peer.addr && *s == "live")
        })
        .await?;
        clear_tracker.wait_for_announces(1, WAIT).await?;

        // PEX to the clearnet peer comes 10 seconds after the connection.
        wait_for("PEX to the clearnet peer", || !clear_peer.pex().is_empty()).await?;
        assert!(
            peers(&node.handle)
                .iter()
                .any(|(a, _, s)| is_synthetic(a) && *s == "live"),
            "the I2P peer went away before the PEX: the test proves nothing"
        );
        for msg in clear_peer.pex() {
            let pex: UtPex<ByteBuf> =
                bencode::from_bytes(&msg).map_err(|e| anyhow::anyhow!("decoding PEX: {e}"))?;
            for p in pex.added_peers().chain(pex.dropped_peers()) {
                assert!(
                    !is_synthetic(&p.addr),
                    "PEX told a clearnet peer {}",
                    p.addr
                );
            }
        }
        for a in clear_tracker.announces() {
            assert!(
                !leaks_i2p(&a),
                "a clearnet tracker request leaks I2P: {a:?}"
            );
        }
        assert!(!tracker.announces().is_empty());

        // PEX is offered to the clearnet peer, not to the I2P one.
        let hs = |p: &WirePeer| -> anyhow::Result<Option<u8>> {
            let raw = p.extended_handshake().context("no extended handshake")?;
            let hs: ExtendedHandshake<ByteBuf> = bencode::from_bytes(&raw)
                .map_err(|e| anyhow::anyhow!("decoding the extended handshake: {e}"))?;
            Ok(hs.m.ut_pex)
        };
        assert!(
            hs(&clear_peer)?.is_some(),
            "no PEX offered to a clearnet peer"
        );
        assert_eq!(hs(&i2p_peer)?, None, "PEX offered to an I2P peer");
        assert!(i2p_peer.pex().is_empty());
        assert_no_violations(&bridge);
        Ok(())
    })
    .await
}

/// A clearnet torrent of a session with I2P never goes through the bridge, and downloads as
/// usual next to an I2P torrent.
#[tokio::test(flavor = "multi_thread")]
async fn test_clearnet_torrent_never_touches_the_bridge() -> anyhow::Result<()> {
    run(async {
        let bridge = FakeSamBridge::start().await?;
        let tracker = I2pTracker::start(&bridge).await?;
        let clear_tracker = FakeTracker::start().await?;
        let i2p_content = content(4).await?;
        let clear_content = content(4).await?;
        let (_seeder, seeder_sid) =
            i2p_seeder(&bridge, &tracker, &i2p_content, NodeOpts::default()).await?;
        let clear_seeder = Node::start(
            &clear_content,
            NodeOpts {
                seed: true,
                listen: true,
                ..Default::default()
            },
        )
        .await?;
        // The seeder is settled (session, accept, announce): any later bridge connection is
        // the subject's.
        wait_for("the seeder's STREAM ACCEPT", || {
            stream_accepts_of(&bridge, &seeder_sid) > 0
        })
        .await?;
        let connections_before = bridge.connections();

        let node = Node::start(
            &clear_content,
            NodeOpts {
                i2p: i2p(&bridge, false),
                trackers: vec![clear_tracker.url()],
                initial_peers: vec![clear_seeder.addr()],
                ..Default::default()
            },
        )
        .await?;
        node.wait_complete(&clear_content).await?;
        assert_eq!(
            bridge.connections(),
            connections_before,
            "the clearnet torrent went to the bridge"
        );
        assert_eq!(bridge.session_history().len(), 1);

        // Now an I2P torrent next to it.
        let dir = TempDir::with_prefix("rqbit_i2p_second")?;
        let i2p_handle = Node::add(
            &node.session,
            &i2p_content,
            &dir,
            &TorrentOpts {
                seed: false,
                trackers: vec![tracker.url()],
                initial_peers: Vec::new(),
                force_tracker_interval: None,
            },
        )
        .await?;
        timeout(WAIT, i2p_handle.wait_until_completed())
            .await
            .context("the I2P download did not complete")??;
        assert_eq!(
            std::fs::read(dir.path().join("out").join("0.data"))?,
            i2p_content.data
        );

        // The bridge saw the I2P torrent only: our own destination, the tracker and the seeder.
        let seeder_dest = bridge.session_history()[0].destination.to_i2p_base64();
        let tracker_dest = tracker.dest.to_i2p_base64();
        for cmd in parsed_commands(&bridge) {
            match (cmd.topic.as_str(), cmd.kind.as_str()) {
                ("NAMING", "LOOKUP") => {
                    let name = cmd.get("NAME").unwrap_or_default();
                    assert!(["ME", TRACKER_NAME].contains(&name), "NAMING LOOKUP {name}");
                }
                ("STREAM", "CONNECT") => {
                    let d = cmd.get("DESTINATION").unwrap_or_default();
                    assert!(d == seeder_dest || d == tracker_dest, "STREAM CONNECT {d}");
                }
                _ => {}
            }
        }
        for a in clear_tracker.announces() {
            assert!(!leaks_i2p(&a), "{a:?}");
        }
        assert!(!clear_tracker.announces().is_empty());
        assert!(peers(&node.handle).iter().all(|(a, ..)| !is_synthetic(a)));
        assert!(node.session.stats_snapshot().connections.tcp.v4.successes > 0);
        assert_no_violations(&bridge);
        Ok(())
    })
    .await
}

/// The bridge goes away in the middle of a download and comes back on the same address: the
/// sessions create new SAM sessions (new destinations), announce them and finish.
#[tokio::test(flavor = "multi_thread")]
async fn test_download_survives_a_bridge_restart() -> anyhow::Result<()> {
    run(async {
        let bridge = FakeSamBridge::start().await?;
        let tracker = I2pTracker::start(&bridge).await?;
        let c = content(16).await?;
        let reannounce = Some(Duration::from_millis(200));
        let (_seeder, _) = i2p_seeder(
            &bridge,
            &tracker,
            &c,
            NodeOpts {
                upload_bps: Some(128 * 1024),
                force_tracker_interval: reannounce,
                ..Default::default()
            },
        )
        .await?;
        let leecher = Node::start(
            &c,
            NodeOpts {
                i2p: i2p(&bridge, false),
                trackers: vec![tracker.url()],
                force_tracker_interval: reannounce,
                ..Default::default()
            },
        )
        .await?;
        wait_for("some progress", || {
            leecher.handle.stats().progress_bytes > 0
        })
        .await?;
        bridge.go_down();
        assert!(
            !leecher.handle.stats().finished,
            "finished before the bridge went down"
        );
        let old: HashSet<String> = bridge
            .session_history()
            .iter()
            .map(|s| s.destination.to_i2p_base64())
            .collect();
        assert_eq!(old.len(), 2);
        // Both supervisors try again while the bridge is down.
        wait_for("attempts while down", || bridge.refused() >= 2).await?;
        bridge.go_up();

        leecher.wait_complete(&c).await?;
        let live: Vec<String> = bridge
            .sessions()
            .iter()
            .map(|s| s.destination.to_i2p_base64())
            .collect();
        assert_eq!(live.len(), 2, "{live:?}");
        let announcers = tracker.announcers();
        for d in &live {
            assert!(!old.contains(d), "a destination came back");
            assert!(
                announcers.contains(&format!("{d}.i2p")),
                "the new destination was not announced"
            );
        }
        assert_no_violations(&bridge);
        Ok(())
    })
    .await
}

/// Without I2P in the session, an `.i2p` tracker says why it is not used and a synthetic
/// address is never dialed.
#[tokio::test(flavor = "multi_thread")]
async fn test_without_i2p_i2p_parts_fail_cleanly() -> anyhow::Result<()> {
    run(async {
        let c = content(2).await?;
        let synthetic = synthetic_addr(1);
        let node = Node::start(
            &c,
            NodeOpts {
                trackers: vec![format!("http://{TRACKER_NAME}/announce")],
                initial_peers: vec![synthetic],
                ..Default::default()
            },
        )
        .await?;
        wait_for("the error of the I2P tracker", || {
            node.handle
                .trackers()
                .iter()
                .any(|t| t.last_error.as_deref() == Some(I2P_NOT_ENABLED))
        })
        .await?;
        assert!(node.handle.live().is_some(), "the torrent is not running");
        assert!(node.handle.stats().error.is_none());
        let conns = node.session.stats_snapshot().connections;
        assert_eq!(conns.i2p.attempts, 0);
        assert_eq!(conns.tcp.v4.attempts + conns.tcp.v6.attempts, 0);
        assert!(peers(&node.handle).iter().all(|(a, ..)| !is_synthetic(a)));

        // Dialed anyway, a synthetic address is an error, not a connection anywhere.
        let connector = StreamConnector::new(StreamConnectorArgs {
            enable_tcp: true,
            ..Default::default()
        })
        .await?;
        let err = connector
            .connect(synthetic)
            .await
            .err()
            .context("a synthetic address was dialed")?;
        assert!(err.to_string().contains("I2P is not enabled"), "{err:#}");
        let s = connector.stats();
        assert_eq!(
            s.tcp.v4.attempts.load(SeqCst) + s.tcp.v6.attempts.load(SeqCst),
            0
        );
        Ok(())
    })
    .await
}

/// Silent I2P peers: listeners behind bridge services that count their connections.
struct SilentPeers {
    open: Arc<AtomicUsize>,
    max_open: Arc<AtomicUsize>,
    accepted: Arc<AtomicUsize>,
    close: Arc<Mutex<Vec<CancellationToken>>>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for SilentPeers {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl SilentPeers {
    async fn start(n: usize, bridge: &FakeSamBridge, tracker: &I2pTracker) -> anyhow::Result<Self> {
        let open = Arc::new(AtomicUsize::new(0));
        let max_open = Arc::new(AtomicUsize::new(0));
        let accepted = Arc::new(AtomicUsize::new(0));
        let close: Arc<Mutex<Vec<CancellationToken>>> = Default::default();
        let mut tasks = Vec::with_capacity(n);
        for _ in 0..n {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
            tracker.add_peer(&bridge.add_service(None, listener.local_addr()?));
            let (open, max_open, accepted, close) = (
                open.clone(),
                max_open.clone(),
                accepted.clone(),
                close.clone(),
            );
            tasks.push(tokio::spawn(async move {
                while let Ok((mut sock, _)) = listener.accept().await {
                    accepted.fetch_add(1, SeqCst);
                    max_open.fetch_max(open.fetch_add(1, SeqCst) + 1, SeqCst);
                    let token = CancellationToken::new();
                    close.lock().push(token.clone());
                    let open = open.clone();
                    tokio::spawn(async move {
                        let mut buf = [0u8; 1024];
                        loop {
                            tokio::select! {
                                _ = token.cancelled() => break,
                                r = sock.read(&mut buf) => if matches!(r, Ok(0) | Err(_)) {
                                    break;
                                },
                            }
                        }
                        open.fetch_sub(1, SeqCst);
                    });
                }
            }));
        }
        Ok(Self {
            open,
            max_open,
            accepted,
            close,
            tasks,
        })
    }
}

/// The peer limit of a torrent counts I2P connections.
#[tokio::test(flavor = "multi_thread")]
async fn test_peer_limit_applies_to_i2p_peers() -> anyhow::Result<()> {
    run(async {
        let bridge = FakeSamBridge::start().await?;
        let tracker = I2pTracker::start(&bridge).await?;
        let c = content(2).await?;
        let swarm = SilentPeers::start(4, &bridge, &tracker).await?;
        let node = Node::start(
            &c,
            NodeOpts {
                i2p: i2p(&bridge, false),
                trackers: vec![tracker.url()],
                peer_limit: Some(2),
                ..Default::default()
            },
        )
        .await?;
        wait_for("2 connections, 4 peers known", || {
            swarm.open.load(SeqCst) == 2 && peers(&node.handle).len() == 4
        })
        .await?;
        assert_eq!(swarm.max_open.load(SeqCst), 2, "over the limit");

        // A connection ends: a waiting peer takes its place, still within the limit.
        let first = swarm
            .close
            .lock()
            .first()
            .cloned()
            .context("no connection")?;
        first.cancel();
        wait_for("a third peer dialed", || {
            swarm.accepted.load(SeqCst) == 3 && swarm.open.load(SeqCst) == 2
        })
        .await?;
        assert_eq!(swarm.max_open.load(SeqCst), 2, "over the limit");
        assert!(peers(&node.handle).iter().all(|(a, ..)| is_synthetic(a)));
        assert_no_violations(&bridge);
        Ok(())
    })
    .await
}
