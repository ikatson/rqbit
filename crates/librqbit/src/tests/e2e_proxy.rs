//! Sessions behind a proxy, against the fake proxies of `librqbit_core::proxy::test_util` on
//! loopback. Every test checks both that the traffic works and that the destination only ever
//! saw the proxy: the proxy connects and relays datagrams from its own sockets, so a source
//! address a seeder, a tracker or a DHT node sees that is not one of them would be direct
//! traffic from the client.

use std::{
    collections::HashSet,
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;
use librqbit_core::proxy::test_util::{FakeProxy, FakeProxyKind, FakeProxyOptions};
use parking_lot::Mutex;
use tempfile::TempDir;
use tokio::{net::UdpSocket, task::JoinHandle, time::timeout};

use crate::{
    AddTorrent, AddTorrentOptions, ConnectionOptions, DhtSessionConfig, ListenerMode,
    ListenerOptions, PeerBackoffConfig, Session, SessionOptions, TrackerState,
    stream_connect::ConnectionKind,
    tests::{
        e2e_trackers::FakeTracker,
        fake_peer::{Fixture, SeederOpts, run, start_seeder},
    },
    torrent_state::live::peer::stats::snapshot::{PeerStatsFilter, PeerStatsFilterState},
};

const WAIT: Duration = Duration::from_secs(30);

/// Retries a dead peer quickly, so that a test can watch a session recover.
const FAST_BACKOFF: PeerBackoffConfig = PeerBackoffConfig {
    min_delay: Duration::from_millis(100),
    factor: 1.5,
    max_delay: Duration::from_millis(500),
    total_delay: Some(Duration::from_secs(600)),
};

async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) -> anyhow::Result<()> {
    let deadline = Instant::now() + WAIT;
    while !cond() {
        anyhow::ensure!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

struct ClientSetup {
    connect: ConnectionOptions,
    listen: Option<ListenerOptions>,
    /// The trackers of the torrent.
    trackers: Vec<String>,
    dht: Option<DhtSessionConfig>,
}

impl ClientSetup {
    fn proxy(url: String) -> Self {
        Self {
            connect: ConnectionOptions {
                proxy_url: Some(url),
                ..Default::default()
            },
            listen: None,
            trackers: Vec::new(),
            dht: None,
        }
    }
}

/// A DHT without persistence that knows only `bootstrap`.
fn dht_config(bootstrap: SocketAddr) -> DhtSessionConfig {
    DhtSessionConfig {
        bootstrap_addrs: Some(vec![bootstrap.to_string()]),
        port: None,
        persistence: None,
        enforce_node_id: false,
    }
}

/// A UDP tracker (BEP 15) that answers every announce with no peers and a long interval, and
/// records the action and the source of every request.
struct FakeUdpTracker {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<(u32, SocketAddr)>>>,
    task: JoinHandle<()>,
}

impl Drop for FakeUdpTracker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeUdpTracker {
    async fn start() -> anyhow::Result<Self> {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = socket.local_addr()?;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let task = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            loop {
                let Ok((len, from)) = socket.recv_from(&mut buf).await else {
                    return;
                };
                if len < 16 {
                    continue;
                }
                let action = u32::from_be_bytes(buf[8..12].try_into().unwrap());
                let tid = &buf[12..16];
                recorded.lock().push((action, from));
                let mut out = Vec::with_capacity(20);
                out.extend_from_slice(&action.to_be_bytes());
                out.extend_from_slice(tid);
                match action {
                    // connect: the connection id
                    0 => out.extend_from_slice(&0x1234_5678_9abc_def0u64.to_be_bytes()),
                    // announce: interval, leechers, seeders, no peers
                    1 => {
                        for v in [1800u32, 0, 0] {
                            out.extend_from_slice(&v.to_be_bytes());
                        }
                    }
                    // scrape: seeders, completed, leechers of the one torrent
                    2 => out.extend_from_slice(&[0u8; 12]),
                    _ => continue,
                }
                let _ = socket.send_to(&out, from).await;
            }
        });
        Ok(Self {
            addr,
            requests,
            task,
        })
    }

    fn url(&self) -> String {
        format!("udp://{}/announce", self.addr)
    }

    fn announces(&self) -> usize {
        self.requests.lock().iter().filter(|(a, _)| *a == 1).count()
    }

    fn sources(&self) -> Vec<SocketAddr> {
        self.requests.lock().iter().map(|(_, from)| *from).collect()
    }
}

/// The sources of the datagrams `socket` got so far.
fn drain_sources(socket: &UdpSocket) -> Vec<SocketAddr> {
    let mut buf = [0u8; 2048];
    let mut sources = Vec::new();
    while let Ok((_, from)) = socket.try_recv_from(&mut buf) {
        sources.push(from);
    }
    sources
}

/// Checks that `sources` (of datagrams a destination got) are all relay sockets of `proxy`.
fn assert_only_relayed(what: &str, sources: &[SocketAddr], proxy: &FakeProxy) {
    let relays: HashSet<SocketAddr> = proxy.datagrams().iter().map(|d| d.from).collect();
    assert!(!sources.is_empty(), "{what} got nothing");
    assert!(
        sources.iter().all(|s| relays.contains(s)),
        "{what} was reached directly: sources {sources:?}, relays {relays:?}"
    );
}

/// A session that downloads the torrent of `seed` and knows it as its only peer.
async fn start_client(seed: &Fixture, setup: ClientSetup) -> anyhow::Result<Fixture> {
    let dir = TempDir::with_prefix("rqbit_proxy_client")?;
    let session = Session::new_with_opts(
        dir.path().into(),
        SessionOptions {
            dht: setup.dht,
            persistence: None,
            listen: setup.listen,
            connect: Some(setup.connect),
            ipv4_only: true,
            disable_local_service_discovery: true,
            peer_backoff: FAST_BACKOFF,
            ..Default::default()
        },
    )
    .await
    .context("error creating client session")?;
    let handle = session
        .add_torrent(
            AddTorrent::from_bytes(seed.torrent_bytes.clone()),
            Some(AddTorrentOptions {
                paused: false,
                initial_peers: Some(vec![seed.addr()]),
                trackers: (!setup.trackers.is_empty()).then_some(setup.trackers),
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .context("into_handle")?;
    handle.wait_until_initialized().await?;
    let addr = session.listen_addr();
    Ok(Fixture {
        session,
        handle,
        addr,
        info_hash: seed.info_hash,
        torrent_bytes: seed.torrent_bytes.clone(),
        content: Vec::new(),
        _dir: dir,
    })
}

/// The peer addresses `f` knows about, connected or not.
fn known_peers(f: &Fixture) -> Vec<SocketAddr> {
    f.handle.live().map_or_else(Vec::new, |l| {
        l.per_peer_stats_snapshot(PeerStatsFilter {
            state: PeerStatsFilterState::All,
        })
        .peers
        .keys()
        .filter_map(|k| k.parse().ok())
        .collect()
    })
}

/// How many connection errors `client` had with `seed`.
fn errors_with_seeder(client: &Fixture, seed: &Fixture) -> u32 {
    client.handle.live().map_or(0, |l| {
        l.per_peer_stats_snapshot(PeerStatsFilter {
            state: PeerStatsFilterState::All,
        })
        .peers
        .get(&seed.addr().to_string())
        .map_or(0, |p| p.counters.errors)
    })
}

/// How many outgoing connections of `kind` the session of `client` made (IPv4, all tests are
/// on IPv4 loopback).
fn connections_made(client: &Fixture, kind: ConnectionKind) -> u64 {
    let c = client.session.stats_snapshot().connections;
    match kind {
        ConnectionKind::Tcp => c.tcp.v4.successes,
        ConnectionKind::Utp => c.utp.v4.successes,
        ConnectionKind::Socks => c.socks.v4.successes,
        ConnectionKind::HttpProxy => c.http_proxy.v4.successes,
    }
}

/// Checks that every connection `seed` got came from the proxy.
fn assert_only_proxy_reached(seed: &Fixture, proxy: &FakeProxy) {
    let egress = proxy.egress_addrs();
    let peers = known_peers(seed);
    assert!(!peers.is_empty(), "the seeder saw no connection at all");
    for p in peers {
        assert!(
            egress.contains(&p),
            "the seeder got a connection from {p}, which is not the proxy ({egress:?})"
        );
    }
    assert!(
        proxy
            .connections()
            .iter()
            .all(|c| c.target == seed.addr().to_string()),
        "the proxy was asked to connect elsewhere: {:?}",
        proxy.connections()
    );
}

/// Checks that the client made no direct outgoing connection.
fn assert_no_direct_connection(client: &Fixture) {
    let c = client.session.stats_snapshot().connections;
    for (name, s) in [
        ("TCP v4", &c.tcp.v4),
        ("TCP v6", &c.tcp.v6),
        ("uTP v4", &c.utp.v4),
        ("uTP v6", &c.utp.v6),
    ] {
        assert_eq!(s.attempts, 0, "{name} connections were attempted");
    }
}

async fn download_through(opts: FakeProxyOptions, kind: ConnectionKind) -> anyhow::Result<()> {
    let seed = start_seeder(SeederOpts::default()).await?;
    let proxy = FakeProxy::start(opts).await;
    let client = start_client(&seed, ClientSetup::proxy(proxy.url())).await?;
    timeout(WAIT, client.handle.wait_until_completed())
        .await
        .context("download through the proxy did not complete")??;

    assert!(connections_made(&client, kind) > 0, "no {kind} connection");
    assert_only_proxy_reached(&seed, &proxy);
    assert_no_direct_connection(&client);
    Ok(())
}

fn with_credentials(kind: FakeProxyKind, user: &str, pass: &str) -> FakeProxyOptions {
    FakeProxyOptions {
        credentials: Some((user.to_owned(), pass.to_owned())),
        ..FakeProxyOptions::new(kind)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_download_through_socks5() -> anyhow::Result<()> {
    run(download_through(
        FakeProxyOptions::new(FakeProxyKind::Socks5),
        ConnectionKind::Socks,
    ))
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_download_through_socks5h_with_password() -> anyhow::Result<()> {
    run(download_through(
        FakeProxyOptions {
            remote_dns: true,
            // Reserved characters reach the proxy percent-decoded.
            ..with_credentials(FakeProxyKind::Socks5, "us@r", "p:ss/w%rd")
        },
        ConnectionKind::Socks,
    ))
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_download_through_socks4() -> anyhow::Result<()> {
    run(download_through(
        FakeProxyOptions::new(FakeProxyKind::Socks4),
        ConnectionKind::Socks,
    ))
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_download_through_socks4a_with_user_id() -> anyhow::Result<()> {
    run(download_through(
        FakeProxyOptions {
            remote_dns: true,
            ..with_credentials(FakeProxyKind::Socks4, "rqbit", "")
        },
        ConnectionKind::Socks,
    ))
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_download_through_http_connect() -> anyhow::Result<()> {
    run(download_through(
        FakeProxyOptions::new(FakeProxyKind::Http),
        ConnectionKind::HttpProxy,
    ))
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_download_through_http_connect_with_password() -> anyhow::Result<()> {
    run(download_through(
        with_credentials(FakeProxyKind::Http, "user", "s3cr:t@"),
        ConnectionKind::HttpProxy,
    ))
    .await
}

/// A rejected password is a connection error to the peer: nothing reaches the seeder, nothing
/// goes out directly, and the session itself keeps running.
async fn wrong_password(kind: FakeProxyKind) -> anyhow::Result<()> {
    let seed = start_seeder(SeederOpts::default()).await?;
    let proxy = FakeProxy::start(with_credentials(kind, "user", "right")).await;
    let client = start_client(
        &seed,
        ClientSetup::proxy(proxy.url_with_credentials("user", "wrong")),
    )
    .await?;
    wait_for("two rejected attempts", || {
        proxy.auth_failures() >= 2 && errors_with_seeder(&client, &seed) >= 2
    })
    .await?;

    assert_eq!(
        proxy.connections(),
        vec![],
        "the proxy relayed a connection"
    );
    assert_eq!(known_peers(&seed), vec![], "the seeder was reached");
    assert_no_direct_connection(&client);
    assert_eq!(client.handle.stats().progress_bytes, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_socks5_wrong_password_fails_cleanly() -> anyhow::Result<()> {
    run(wrong_password(FakeProxyKind::Socks5)).await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_http_wrong_password_fails_cleanly() -> anyhow::Result<()> {
    run(wrong_password(FakeProxyKind::Http)).await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_refusing_proxy_is_not_bypassed() -> anyhow::Result<()> {
    run(async {
        for kind in [
            FakeProxyKind::Socks5,
            FakeProxyKind::Socks4,
            FakeProxyKind::Http,
        ] {
            let seed = start_seeder(SeederOpts::default()).await?;
            let proxy = FakeProxy::start(FakeProxyOptions {
                refuse: true,
                ..FakeProxyOptions::new(kind)
            })
            .await;
            let client = start_client(&seed, ClientSetup::proxy(proxy.url())).await?;
            wait_for("two refused attempts", || {
                proxy.refused() >= 2 && errors_with_seeder(&client, &seed) >= 2
            })
            .await?;
            assert_eq!(
                known_peers(&seed),
                vec![],
                "{kind:?}: the seeder was reached"
            );
            assert_no_direct_connection(&client);
        }
        Ok(())
    })
    .await
}

/// The state and the last error of every tracker of `client`.
fn tracker_rows(client: &Fixture) -> Vec<(TrackerState, Option<String>)> {
    client
        .handle
        .trackers()
        .into_iter()
        .map(|r| (r.state, r.last_error))
        .collect()
}

/// UDP trackers and the DHT go through the SOCKS5 proxy (UDP ASSOCIATE) like the peers: the
/// tracker and the DHT node only ever see the proxy's relay sockets.
#[tokio::test(flavor = "multi_thread")]
async fn test_udp_tracker_and_dht_through_socks5() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let proxy = FakeProxy::start(FakeProxyOptions::new(FakeProxyKind::Socks5)).await;
        let tracker = FakeUdpTracker::start().await?;
        let dht_node = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let client = start_client(
            &seed,
            ClientSetup {
                trackers: vec![tracker.url()],
                dht: Some(dht_config(dht_node.local_addr()?)),
                ..ClientSetup::proxy(proxy.url())
            },
        )
        .await?;
        let dht = client.session.get_dht().context("no DHT")?;
        assert!(dht.proxy().is_some(), "the DHT is not behind the proxy");

        timeout(WAIT, client.handle.wait_until_completed())
            .await
            .context("download through the proxy did not complete")??;
        wait_for("an announce", || tracker.announces() > 0).await?;
        let mut dht_sources = Vec::new();
        wait_for("the DHT bootstrap", || {
            dht_sources.extend(drain_sources(&dht_node));
            !dht_sources.is_empty()
        })
        .await?;

        assert_only_relayed("the UDP tracker", &tracker.sources(), &proxy);
        assert_only_relayed("the DHT node", &dht_sources, &proxy);
        assert_only_proxy_reached(&seed, &proxy);
        assert_no_direct_connection(&client);
        Ok(())
    })
    .await
}

/// Behind proxies that can't relay UDP (HTTP CONNECT, SOCKS4) the peers still go through the
/// proxy, the DHT is off and the UDP tracker shows the error: nothing goes out directly.
#[tokio::test(flavor = "multi_thread")]
async fn test_tcp_only_proxies_turn_udp_off() -> anyhow::Result<()> {
    run(async {
        for kind in [FakeProxyKind::Http, FakeProxyKind::Socks4] {
            let seed = start_seeder(SeederOpts::default()).await?;
            let proxy = FakeProxy::start(FakeProxyOptions::new(kind)).await;
            let tracker = FakeUdpTracker::start().await?;
            let dht_node = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;
            let client = start_client(
                &seed,
                ClientSetup {
                    trackers: vec![tracker.url()],
                    dht: Some(dht_config(dht_node.local_addr()?)),
                    ..ClientSetup::proxy(proxy.url())
                },
            )
            .await?;
            assert!(client.session.get_dht().is_none(), "{kind:?}: DHT is on");

            timeout(WAIT, client.handle.wait_until_completed())
                .await
                .with_context(|| format!("{kind:?}: download did not complete"))??;
            wait_for("the tracker error", || {
                tracker_rows(&client)
                    .iter()
                    .any(|(s, e)| *s == TrackerState::Error && e.is_some())
            })
            .await?;
            let (_, error) = tracker_rows(&client).remove(0);
            let error = error.unwrap_or_default();
            assert!(error.contains("cannot relay UDP"), "{kind:?}: {error}");

            assert_eq!(
                tracker.sources(),
                vec![],
                "{kind:?}: the tracker was reached"
            );
            assert_eq!(drain_sources(&dht_node), vec![], "{kind:?}: DHT traffic");
            assert!(proxy.datagrams().is_empty());
            assert_only_proxy_reached(&seed, &proxy);
            assert_no_direct_connection(&client);
        }
        Ok(())
    })
    .await
}

/// With the proxy down nothing goes out: the seeder, the UDP tracker and the DHT node get
/// nothing and the errors show. Once the proxy is back the download, the announces and the DHT
/// go on through it (new UDP associations), without restarting anything.
#[tokio::test(flavor = "multi_thread")]
async fn test_session_recovers_when_the_proxy_comes_back() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let proxy = FakeProxy::start(FakeProxyOptions::new(FakeProxyKind::Socks5)).await;
        let tracker = FakeUdpTracker::start().await?;
        let dht_node = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        proxy.stop().await;
        let client = start_client(
            &seed,
            ClientSetup {
                trackers: vec![tracker.url()],
                dht: Some(dht_config(dht_node.local_addr()?)),
                ..ClientSetup::proxy(proxy.url())
            },
        )
        .await?;
        wait_for("two failed attempts", || {
            errors_with_seeder(&client, &seed) >= 2
        })
        .await?;
        wait_for("the tracker error", || {
            tracker_rows(&client)
                .iter()
                .any(|(s, e)| *s == TrackerState::Error && e.is_some())
        })
        .await?;
        assert_eq!(known_peers(&seed), vec![], "the seeder was reached");
        assert_eq!(tracker.sources(), vec![], "the tracker was reached");
        assert_eq!(drain_sources(&dht_node), vec![], "the DHT node was reached");
        assert_no_direct_connection(&client);

        proxy.restart().await;
        timeout(WAIT, client.handle.wait_until_completed())
            .await
            .context("the download did not resume through the proxy")??;
        let deadline = Instant::now() + WAIT;
        while tracker.announces() == 0 {
            anyhow::ensure!(Instant::now() < deadline, "no announce after the restart");
            // The failed tracker is retried on its own only after its backoff.
            client.session.reannounce(&client.handle)?;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let mut dht_sources = Vec::new();
        wait_for("the DHT bootstrap", || {
            dht_sources.extend(drain_sources(&dht_node));
            !dht_sources.is_empty()
        })
        .await?;
        assert_only_relayed("the UDP tracker", &tracker.sources(), &proxy);
        assert_only_relayed("the DHT node", &dht_sources, &proxy);
        assert_only_proxy_reached(&seed, &proxy);
        assert_no_direct_connection(&client);
        Ok(())
    })
    .await
}

/// With `proxy_peer_connections` off a proxy is still set: peers and the DHT go directly, the
/// trackers (HTTP and UDP) still through the proxy.
#[tokio::test(flavor = "multi_thread")]
async fn test_proxy_peer_connections_off_connects_directly() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let proxy = FakeProxy::start(FakeProxyOptions::new(FakeProxyKind::Socks5)).await;
        let http_tracker = FakeTracker::start().await?;
        let udp_tracker = FakeUdpTracker::start().await?;
        let dht_node = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let client = start_client(
            &seed,
            ClientSetup {
                connect: ConnectionOptions {
                    proxy_url: Some(proxy.url()),
                    proxy_peer_connections: false,
                    ..Default::default()
                },
                trackers: vec![http_tracker.url(), udp_tracker.url()],
                dht: Some(dht_config(dht_node.local_addr()?)),
                ..ClientSetup::proxy(proxy.url())
            },
        )
        .await?;
        let dht = client.session.get_dht().context("no DHT")?;
        assert!(dht.proxy().is_none(), "the DHT is behind the proxy");

        timeout(WAIT, client.handle.wait_until_completed())
            .await
            .context("direct download did not complete")??;
        assert!(connections_made(&client, ConnectionKind::Tcp) > 0);
        assert_eq!(connections_made(&client, ConnectionKind::Socks), 0);

        http_tracker.wait_for_announces(1, WAIT).await?;
        wait_for("a UDP announce", || udp_tracker.announces() > 0).await?;
        let mut dht_sources = Vec::new();
        wait_for("the DHT bootstrap", || {
            dht_sources.extend(drain_sources(&dht_node));
            !dht_sources.is_empty()
        })
        .await?;

        let targets: Vec<String> = proxy.connections().into_iter().map(|c| c.target).collect();
        assert!(
            targets.contains(&http_tracker.addr.to_string()),
            "the HTTP tracker was not reached through the proxy: {targets:?}"
        );
        assert!(
            !targets.contains(&seed.addr().to_string()),
            "the proxy was used for a peer"
        );
        assert_only_relayed("the UDP tracker", &udp_tracker.sources(), &proxy);
        let relays: HashSet<SocketAddr> = proxy.datagrams().iter().map(|d| d.from).collect();
        assert!(
            dht_sources.iter().all(|s| !relays.contains(s)),
            "the DHT went through the proxy"
        );
        Ok(())
    })
    .await
}

/// uTP can't go through a proxy: a session with a uTP listener behind a proxy still connects
/// out over TCP through the proxy only, and its listener keeps accepting incoming uTP.
#[tokio::test(flavor = "multi_thread")]
async fn test_no_outgoing_utp_behind_a_proxy() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let proxy = FakeProxy::start(FakeProxyOptions::new(FakeProxyKind::Socks5)).await;
        let client = start_client(
            &seed,
            ClientSetup {
                listen: Some(ListenerOptions {
                    mode: ListenerMode::TcpAndUtp,
                    listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
                    ipv4_only: true,
                    ..Default::default()
                }),
                ..ClientSetup::proxy(proxy.url())
            },
        )
        .await?;
        timeout(WAIT, client.handle.wait_until_completed())
            .await
            .context("download through the proxy did not complete")??;
        assert!(connections_made(&client, ConnectionKind::Socks) > 0);
        assert_only_proxy_reached(&seed, &proxy);
        assert_no_direct_connection(&client);

        // The listener is untouched: a session without a proxy can download from the client
        // over uTP only.
        let listen_addr = client.addr();
        let dir = TempDir::with_prefix("rqbit_proxy_utp_leecher")?;
        let leecher = Session::new_with_opts(
            dir.path().into(),
            SessionOptions {
                dht: None,
                persistence: None,
                listen: Some(ListenerOptions {
                    mode: ListenerMode::UtpOnly,
                    listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
                    ipv4_only: true,
                    ..Default::default()
                }),
                connect: Some(ConnectionOptions {
                    enable_tcp: false,
                    ..Default::default()
                }),
                ipv4_only: true,
                disable_local_service_discovery: true,
                ..Default::default()
            },
        )
        .await?;
        let handle = leecher
            .add_torrent(
                AddTorrent::from_bytes(seed.torrent_bytes.clone()),
                Some(AddTorrentOptions {
                    initial_peers: Some(vec![listen_addr]),
                    ..Default::default()
                }),
            )
            .await?
            .into_handle()
            .context("into_handle")?;
        timeout(WAIT, handle.wait_until_completed())
            .await
            .context("uTP download from the proxied session did not complete")??;
        Ok(())
    })
    .await
}
