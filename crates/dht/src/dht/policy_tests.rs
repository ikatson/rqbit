use super::*;
use librqbit_core::network_policy::OutboundPolicy;
use std::{net::Ipv4Addr, sync::atomic::AtomicBool};

#[tokio::test]
async fn denied_request_fails_promptly_without_sending_or_leaking_transaction() {
    let bootstrap = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let blocked = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = blocked.local_addr().unwrap();
    let dht = DhtState::with_config(DhtConfig {
        listen_addr: Some((Ipv4Addr::LOCALHOST, 0).into()),
        bootstrap_addrs: Some(vec![bootstrap.local_addr().unwrap().to_string()]),
        outbound_policy: Some(OutboundPolicy::new(move |addr| {
            addr.port() != address.port()
        })),
        ..Default::default()
    })
    .await
    .unwrap();
    let error = tokio::time::timeout(Duration::from_secs(1), dht.request(Request::Ping, address))
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, Error::OutboundPolicy(_)));
    assert!(
        !dht.inflight_by_transaction_id
            .iter()
            .any(|entry| entry.key().1 == address)
    );
    let mut packet = [0; 1024];
    assert!(
        tokio::time::timeout(Duration::from_millis(100), blocked.recv_from(&mut packet))
            .await
            .is_err()
    );
    dht.cancellation_token().cancel();
}

#[tokio::test]
async fn replies_recheck_policy_without_terminating_the_dht_worker() {
    let bootstrap = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let enabled = Arc::new(AtomicBool::new(true));
    let state = enabled.clone();
    let dht = DhtState::with_config(DhtConfig {
        listen_addr: Some((Ipv4Addr::LOCALHOST, 0).into()),
        bootstrap_addrs: Some(vec![bootstrap.local_addr().unwrap().to_string()]),
        outbound_policy: Some(OutboundPolicy::new(move |_| state.load(Ordering::SeqCst))),
        ..Default::default()
    })
    .await
    .unwrap();
    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ping = b"d1:ad2:id20:abcdefghij0123456789e1:q4:ping1:t2:aa1:y1:qe";
    let mut packet = [0; 1024];
    client.send_to(ping, dht.listen_addr()).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), client.recv_from(&mut packet))
            .await
            .unwrap()
            .is_ok()
    );
    enabled.store(false, Ordering::SeqCst);
    client.send_to(ping, dht.listen_addr()).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), client.recv_from(&mut packet))
            .await
            .is_err()
    );
    enabled.store(true, Ordering::SeqCst);
    client.send_to(ping, dht.listen_addr()).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), client.recv_from(&mut packet))
            .await
            .unwrap()
            .is_ok()
    );
    dht.cancellation_token().cancel();
}
