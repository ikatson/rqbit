//! Sessions behind a proxy, against the fake proxies of `librqbit_core::proxy::test_util` on
//! loopback. Every test checks both that the traffic works and that the destination only ever
//! saw the proxy: the proxy connects from its own sockets, so a peer address the seeder knows
//! that is not one of them would be a direct connection from the client.

use std::{
    net::{Ipv4Addr, SocketAddr},
    time::{Duration, Instant},
};

use anyhow::Context;
use librqbit_core::proxy::test_util::{FakeProxy, FakeProxyKind, FakeProxyOptions};
use tempfile::TempDir;
use tokio::time::timeout;

use crate::{
    AddTorrent, AddTorrentOptions, ConnectionOptions, ListenerMode, ListenerOptions,
    PeerBackoffConfig, Session, SessionOptions,
    stream_connect::ConnectionKind,
    tests::fake_peer::{Fixture, SeederOpts, run, start_seeder},
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
}

impl ClientSetup {
    fn proxy(url: String) -> Self {
        Self {
            connect: ConnectionOptions {
                proxy_url: Some(url),
                ..Default::default()
            },
            listen: None,
        }
    }
}

/// A session that downloads the torrent of `seed` and knows it as its only peer.
async fn start_client(seed: &Fixture, setup: ClientSetup) -> anyhow::Result<Fixture> {
    let dir = TempDir::with_prefix("rqbit_proxy_client")?;
    let session = Session::new_with_opts(
        dir.path().into(),
        SessionOptions {
            dht: None,
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

/// With the proxy down nothing goes out; once it is back the download goes on through it,
/// without restarting anything.
#[tokio::test(flavor = "multi_thread")]
async fn test_peer_connections_recover_when_the_proxy_comes_back() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let proxy = FakeProxy::start(FakeProxyOptions::new(FakeProxyKind::Socks5)).await;
        proxy.stop().await;
        let client = start_client(&seed, ClientSetup::proxy(proxy.url())).await?;
        wait_for("two failed attempts", || {
            errors_with_seeder(&client, &seed) >= 2
        })
        .await?;
        assert_eq!(known_peers(&seed), vec![], "the seeder was reached");
        assert_no_direct_connection(&client);

        proxy.restart().await;
        timeout(WAIT, client.handle.wait_until_completed())
            .await
            .context("the download did not resume through the proxy")??;
        assert_only_proxy_reached(&seed, &proxy);
        assert_no_direct_connection(&client);
        Ok(())
    })
    .await
}

/// With `proxy_peer_connections` off, peers are reached directly even though a proxy is set.
#[tokio::test(flavor = "multi_thread")]
async fn test_proxy_peer_connections_off_connects_directly() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let proxy = FakeProxy::start(FakeProxyOptions::new(FakeProxyKind::Socks5)).await;
        let client = start_client(
            &seed,
            ClientSetup {
                connect: ConnectionOptions {
                    proxy_url: Some(proxy.url()),
                    proxy_peer_connections: false,
                    ..Default::default()
                },
                listen: None,
            },
        )
        .await?;
        timeout(WAIT, client.handle.wait_until_completed())
            .await
            .context("direct download did not complete")??;
        assert!(connections_made(&client, ConnectionKind::Tcp) > 0);
        assert_eq!(proxy.accepted(), 0, "the proxy was used for a peer");
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
