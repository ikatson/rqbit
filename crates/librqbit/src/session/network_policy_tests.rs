use super::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

async fn serve_once(listener: &TcpListener, status: &str, body: &[u8]) -> String {
    let (mut socket, _) = listener.accept().await.unwrap();
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        socket.read_exact(&mut byte).await.unwrap();
        request.push(byte[0]);
        assert!(request.len() < 8192);
    }
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    socket.write_all(response.as_bytes()).await.unwrap();
    socket.write_all(body).await.unwrap();
    String::from_utf8(request).unwrap()
}

async fn session(options: SessionOptions) -> (tempfile::TempDir, Arc<Session>) {
    let directory = tempfile::tempdir().unwrap();
    let session = Session::new_with_opts(
        directory.path().into(),
        SessionOptions {
            dht: None,
            disable_local_service_discovery: true,
            ipv4_only: true,
            ..options
        },
    )
    .await
    .unwrap();
    (directory, session)
}

fn client() -> reqwest::Client {
    let headers = reqwest::header::HeaderMap::from_iter([(
        reqwest::header::HeaderName::from_static("x-host-policy"),
        reqwest::header::HeaderValue::from_static("injected"),
    )]);
    reqwest::Client::builder()
        .no_proxy()
        .default_headers(headers)
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap()
}

#[tokio::test]
async fn metadata_fetch_uses_the_injected_http_client() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (_dir, session) = session(SessionOptions {
        http_client: Some(client()),
        ..Default::default()
    })
    .await;
    let url = format!("http://{}/metadata.torrent", listener.local_addr().unwrap());
    let (result, headers) = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(
            session.add_torrent(AddTorrent::from_url(url), None),
            serve_once(&listener, "404 Not Found", b"")
        )
    })
    .await
    .unwrap();
    assert!(result.is_err());
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("x-host-policy: injected")
    );
    session.stop().await;
}

#[tokio::test]
async fn private_tracker_filter_runs_before_selection_and_excludes_global_trackers() {
    let blocked = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let allowed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let global = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = |listener: &TcpListener| {
        url::Url::parse(&format!(
            "http://{}/announce",
            listener.local_addr().unwrap()
        ))
        .unwrap()
    };
    let blocked_port = blocked.local_addr().unwrap().port();
    let checked_ports = Arc::new(std::sync::Mutex::new(Vec::new()));
    let policy_checked_ports = checked_ports.clone();
    let (_dir, session) = session(SessionOptions {
        http_client: Some(client()),
        trackers: HashSet::from([url(&global)]),
        tracker_url_filter: Some(Arc::new(move |url| {
            policy_checked_ports.lock().unwrap().push(url.port());
            url.port() != Some(blocked_port)
        })),
        ..Default::default()
    })
    .await;
    // BEP 52 discovery uses the first 20 bytes of the v2 SHA-256 info-hash.
    let info_hash = Id20::new([7; 20]);
    let mut peers = session
        .make_peer_rx(
            info_hash,
            vec![url(&blocked), url(&allowed)],
            false,
            None,
            vec![],
            true,
        )
        .unwrap();
    let headers = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::select! {
            headers = serve_once(&allowed, "200 OK", b"d8:intervali60e5:peers0:e") => headers,
            _ = peers.next() => panic!("tracker stream ended without contacting the allowed tracker"),
        }
    }).await.unwrap();
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("x-host-policy: injected")
    );
    drop(peers);
    assert_eq!(
        *checked_ports.lock().unwrap(),
        vec![
            Some(blocked_port),
            Some(allowed.local_addr().unwrap().port())
        ]
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), blocked.accept())
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), global.accept())
            .await
            .is_err()
    );
    session.stop().await;
}

#[tokio::test]
async fn rejecting_all_torrent_and_global_trackers_never_starts_discovery() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = url::Url::parse(&format!(
        "http://{}/announce",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let (_dir, session) = session(SessionOptions {
        http_client: Some(client()),
        trackers: HashSet::from([url.clone()]),
        tracker_url_filter: Some(Arc::new(|_| false)),
        ..Default::default()
    })
    .await;
    assert!(
        session
            .make_peer_rx(Id20::new([9; 20]), vec![url], false, None, vec![], false)
            .is_none()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
    session.stop().await;
}

#[tokio::test]
async fn session_passes_the_host_policy_to_all_native_transports() {
    use librqbit_core::network_policy::OutboundPolicy;
    for persistent in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let dht_probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let bootstrap = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let policy_observed = observed.clone();
        let session = Session::new_with_opts(
            directory.path().into(),
            SessionOptions {
                dht: Some(DhtSessionConfig {
                    port: Some(0),
                    bootstrap_addrs: Some(vec![bootstrap.local_addr().unwrap().to_string()]),
                    persistence: persistent.then(|| DhtPersistenceConfig {
                        config_filename: Some(directory.path().join("dht.json")),
                        ..Default::default()
                    }),
                }),
                http_client: Some(client()),
                outbound_policy: Some(OutboundPolicy::new(move |addr| {
                    policy_observed.lock().unwrap().push(addr);
                    false
                })),
                disable_local_service_discovery: true,
                ipv4_only: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            session.connector.connect(tcp.local_addr().unwrap()).await,
            Err(crate::Error::OutboundPolicy(_))
        ));
        let tracker_url = url::Url::parse(&format!("udp://{}", udp.local_addr().unwrap())).unwrap();
        let mut peers = session
            .make_peer_rx(
                Id20::new([3; 20]),
                vec![tracker_url],
                false,
                None,
                vec![],
                true,
            )
            .unwrap();
        // Tracker monitors remain alive for future policy changes; drive the
        // stream until the UDP destination is checked, then drop the monitor.
        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                _ = peers.next() => panic!("unexpected peer or closed tracker monitor"),
                _ = async {
                    while !observed.lock().unwrap().contains(&udp.local_addr().unwrap()) {
                        tokio::task::yield_now().await;
                    }
                } => {},
            }
        })
        .await
        .unwrap();
        drop(peers);
        assert!(
            observed
                .lock()
                .unwrap()
                .contains(&udp.local_addr().unwrap())
        );
        let target = (
            std::net::Ipv4Addr::LOCALHOST,
            session.dht.as_ref().unwrap().listen_addr().port(),
        );
        dht_probe
            .send_to(
                b"d1:ad2:id20:abcdefghij0123456789e1:q4:ping1:t2:aa1:y1:qe",
                target,
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !observed
                .lock()
                .unwrap()
                .contains(&dht_probe.local_addr().unwrap())
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut packet = [0; 1024];
        assert!(
            tokio::time::timeout(Duration::from_millis(100), dht_probe.recv_from(&mut packet))
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), udp.recv_from(&mut packet))
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), tcp.accept())
                .await
                .is_err()
        );
        session.stop().await;
    }
}

#[tokio::test]
async fn restored_dht_preserves_outbound_policy() {
    let bootstrap = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let blocked = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = blocked.local_addr().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("dht.json");
    let bootstrap_addrs = vec![bootstrap.local_addr().unwrap().to_string()];
    // Use the actual persistence writer, rather than duplicating its JSON schema.
    let original = PersistentDht::create(
        DhtPersistenceConfig {
            config_filename: Some(path.clone()),
            dump_interval: Some(Duration::from_millis(10)),
        },
        Some(0),
        true,
        Some(bootstrap_addrs.clone()),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let persisted = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(bytes) = tokio::fs::read(&path).await
                && serde_json::from_slice::<serde_json::Value>(&bytes).is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    original.cancellation_token().cancel();
    persisted.expect("the DHT did not write its routing state");
    let restored = PersistentDht::create(
        DhtPersistenceConfig {
            config_filename: Some(path),
            ..Default::default()
        },
        Some(0),
        true,
        Some(bootstrap_addrs),
        None,
        None,
        Some(crate::OutboundPolicy::new(move |addr| {
            addr.port() != address.port()
        })),
    )
    .await
    .unwrap();
    let target = (std::net::Ipv4Addr::LOCALHOST, restored.listen_addr().port());
    blocked
        .send_to(
            b"d1:ad2:id20:abcdefghij0123456789e1:q4:ping1:t2:aa1:y1:qe",
            target,
        )
        .await
        .unwrap();
    let mut packet = [0; 1024];
    assert!(
        tokio::time::timeout(Duration::from_millis(100), blocked.recv_from(&mut packet))
            .await
            .is_err()
    );
    // A positive control prevents a dead worker from making rejection pass.
    let allowed = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    allowed
        .send_to(
            b"d1:ad2:id20:abcdefghij0123456789e1:q4:ping1:t2:bb1:y1:qe",
            target,
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), allowed.recv_from(&mut packet))
            .await
            .unwrap()
            .is_ok()
    );
    restored.cancellation_token().cancel();
}
