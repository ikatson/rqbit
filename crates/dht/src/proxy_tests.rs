//! The DHT behind a SOCKS5 proxy (UDP ASSOCIATE), against the fake proxies of `librqbit-core`
//! on loopback. A proxied node binds no socket of its own: everything it sends leaves from the
//! proxy's relay socket, and other nodes reach it there.

use std::{
    future::Future,
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

use futures::StreamExt;
use librqbit_core::{
    hash_id::Id20,
    proxy::{
        ProxyConfig, ProxyError, ProxyKind,
        test_util::{FakeProxy, FakeProxyKind, FakeProxyOptions},
    },
};
use tokio::net::UdpSocket;

use crate::{Dht, DhtConfig, DhtState, Error, bprotocol::Want};

const WAIT: Duration = Duration::from_secs(30);

async fn direct_node(bootstrap: SocketAddr) -> Dht {
    DhtState::with_config(DhtConfig {
        listen_addr: Some((Ipv4Addr::LOCALHOST, 0).into()),
        bootstrap_addrs: Some(vec![bootstrap.to_string()]),
        ..Default::default()
    })
    .await
    .unwrap()
}

async fn proxied_node(proxy: &FakeProxy, bootstrap: Vec<String>) -> Dht {
    let config = ProxyConfig::parse(&proxy.url()).unwrap();
    DhtState::with_config(DhtConfig {
        listen_addr: Some((Ipv4Addr::LOCALHOST, 0).into()),
        bootstrap_addrs: Some(bootstrap),
        proxy: Some(&config),
        ..Default::default()
    })
    .await
    .unwrap()
}

/// Retries `f` every 50 ms until it gives `Some`, for at most [`WAIT`].
async fn eventually<T, F: Future<Output = Option<T>>>(what: &str, mut f: impl FnMut() -> F) -> T {
    tokio::time::timeout(WAIT, async {
        loop {
            if let Some(v) = f().await {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

fn stored_ports(node: &Dht, info_hash: Id20) -> Vec<u16> {
    node.peer_store
        .get_for_info_hash(info_hash, Want::V4)
        .into_iter()
        .map(|a| a.0.port())
        .collect()
}

/// Checks that `spy` gets nothing for a while.
async fn assert_silent(spy: &UdpSocket) {
    let mut buf = [0u8; 2048];
    let got = tokio::time::timeout(Duration::from_millis(500), spy.recv_from(&mut buf)).await;
    assert!(got.is_err(), "a datagram went out directly: {got:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn get_peers_and_announce_through_socks5() {
    let silent = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let silent_addr = silent.local_addr().unwrap();
    let proxy = FakeProxy::start(FakeProxyOptions::new(FakeProxyKind::Socks5)).await;
    let b = direct_node(silent_addr).await;
    let c = direct_node(silent_addr).await;
    let a = proxied_node(&proxy, vec![silent_addr.to_string()]).await;
    assert!(a.proxy().is_some());
    assert!(b.proxy().is_none());

    c.add_node(b.listen_addr()).await.unwrap();
    assert_eq!(
        a.add_node(b.listen_addr()).await.unwrap(),
        b.get_stats().id,
        "the proxied node reaches the other one"
    );

    // Close to B, so that B stores the announces.
    let info_hash = b.get_stats().id;
    let _c_announces = c.get_peers(info_hash, Some(5555));
    eventually("C's announce", || async {
        stored_ports(&b, info_hash).contains(&5555).then_some(())
    })
    .await;

    // A finds C's announce, and its own announce reaches B.
    let mut a_peers = a.get_peers(info_hash, Some(4242));
    let found = tokio::time::timeout(WAIT, a_peers.next())
        .await
        .expect("get_peers through the proxy found nothing");
    assert_eq!(found, Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 5555))));
    eventually("A's announce", || async {
        stored_ports(&b, info_hash).contains(&4242).then_some(())
    })
    .await;

    // A only ever spoke from the proxy's relay, and answers there.
    let datagrams = proxy.datagrams();
    assert!(!datagrams.is_empty());
    let relay = datagrams[0].from;
    assert!(datagrams.iter().all(|d| d.from == relay));
    assert_eq!(b.add_node(relay).await.unwrap(), a.get_stats().id);
    assert_eq!(proxy.associations(), 1);
}

#[tokio::test]
async fn proxies_without_udp_are_refused() {
    for url in [
        "http://127.0.0.1:1",
        "socks4://127.0.0.1:1",
        "socks4a://127.0.0.1:1",
    ] {
        let config = ProxyConfig::parse(url).unwrap();
        let result = DhtState::with_config(DhtConfig {
            listen_addr: Some((Ipv4Addr::LOCALHOST, 0).into()),
            bootstrap_addrs: Some(vec![]),
            proxy: Some(&config),
            ..Default::default()
        })
        .await;
        match result {
            Err(Error::Proxy(ProxyError::UdpNotSupported(kind))) => {
                assert_ne!(kind, ProxyKind::Socks5);
            }
            Err(e) => panic!("{url}: unexpected error {e:#}"),
            Ok(_) => panic!("{url}: a DHT was started"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_down_sends_nothing_and_recovers() {
    let silent = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let silent_addr = silent.local_addr().unwrap();
    let proxy = FakeProxy::start(FakeProxyOptions::new(FakeProxyKind::Socks5)).await;
    let b = direct_node(silent_addr).await;
    let a = proxied_node(&proxy, vec![silent_addr.to_string()]).await;
    a.add_node(b.listen_addr()).await.unwrap();

    proxy.stop().await;
    let spy = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let spy_addr = spy.local_addr().unwrap();
    // Once the association is known to be down, requests fail at once with the proxy's error.
    let error = eventually("a proxy error", || async {
        match a.add_node(spy_addr).await {
            Err(Error::Proxy(e)) => Some(e),
            _ => None,
        }
    })
    .await;
    assert!(
        matches!(error, ProxyError::UdpAssociationDown(_)),
        "{error:#}"
    );
    assert_silent(&spy).await;

    // Back without a restart of the DHT: a new association.
    proxy.restart().await;
    eventually("the DHT through the restarted proxy", || async {
        a.add_node(b.listen_addr()).await.ok()
    })
    .await;
    assert!(
        proxy.associations() >= 2,
        "a new association after the restart"
    );
    assert_silent(&spy).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_association_is_rebuilt() {
    let silent = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let silent_addr = silent.local_addr().unwrap();
    let proxy = FakeProxy::start(FakeProxyOptions::new(FakeProxyKind::Socks5)).await;
    let b = direct_node(silent_addr).await;
    let a = proxied_node(&proxy, vec![silent_addr.to_string()]).await;
    a.add_node(b.listen_addr()).await.unwrap();
    assert_eq!(proxy.associations(), 1);

    proxy.drop_udp_associations();
    eventually("a second association", || async {
        (a.add_node(b.listen_addr()).await.is_ok() && proxy.associations() == 2).then_some(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn bootstrap_host_names_follow_the_proxy_dns_mode() {
    // `socks5h`: the proxy is given the name (nothing is looked up here), the node answers
    // through it, and the DHT bootstraps from it: the nodes it knows end up in our table.
    let silent = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let b = direct_node(silent.local_addr().unwrap()).await;
    let c = direct_node(silent.local_addr().unwrap()).await;
    b.add_node(c.listen_addr()).await.unwrap();
    let b_by_name = format!("localhost:{}", b.listen_addr().port());
    let mut opts = FakeProxyOptions::new(FakeProxyKind::Socks5);
    opts.remote_dns = true;
    let remote = FakeProxy::start(opts).await;
    let a = proxied_node(&remote, vec![b_by_name.clone()]).await;
    eventually("a node learnt from the bootstrap node", || async {
        a.with_routing_tables(|v4, _| v4.iter().any(|n| n.addr() == c.listen_addr()))
            .then_some(())
    })
    .await;
    let datagrams = remote.datagrams();
    assert!(
        datagrams.iter().any(|d| d.target == b_by_name),
        "{datagrams:?}"
    );

    let spy = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let by_name = format!("localhost:{}", spy.local_addr().unwrap().port());

    // `socks5`: the name is looked up here, and the node is asked through the proxy.
    let local = FakeProxy::start(FakeProxyOptions::new(FakeProxyKind::Socks5)).await;
    let _a = proxied_node(&local, vec![by_name]).await;
    let mut buf = [0u8; 2048];
    let (_, from) = tokio::time::timeout(WAIT, spy.recv_from(&mut buf))
        .await
        .expect("the bootstrap node was not asked")
        .unwrap();
    let relays: Vec<SocketAddr> = local.datagrams().iter().map(|d| d.from).collect();
    assert!(
        relays.contains(&from),
        "asked from {from}, relays {relays:?}"
    );
}
