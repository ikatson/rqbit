//! The SAM client against the fake bridge.

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{
    fake_bridge::FakeSamBridge,
    sam::{SamConn, SamError, SamResultCode, TunnelOptions},
};

const TUNNELS: TunnelOptions = TunnelOptions {
    inbound_quantity: 3,
    outbound_quantity: 3,
    inbound_length: 3,
    outbound_length: 3,
};

#[tokio::test]
async fn two_sessions_stream_through_the_bridge() {
    let bridge = FakeSamBridge::start().await.unwrap();
    let mut a = SamConn::open(bridge.addr()).await.unwrap();
    a.session_create("a", &TUNNELS).await.unwrap();
    let mut b = SamConn::open(bridge.addr()).await.unwrap();
    b.session_create("b", &TUNNELS).await.unwrap();
    let a_dest = a.naming_lookup("ME").await.unwrap();
    let b_dest = b.naming_lookup("ME").await.unwrap();
    assert_ne!(a_dest, b_dest);

    // The b32 name resolves to the full destination.
    let mut c = SamConn::open(bridge.addr()).await.unwrap();
    assert_eq!(
        c.naming_lookup(&b_dest.hash().b32_name()).await.unwrap(),
        b_dest
    );

    let accept = SamConn::open(bridge.addr())
        .await
        .unwrap()
        .stream_accept("b")
        .await
        .unwrap();
    let (mut ar, mut aw) = SamConn::open(bridge.addr())
        .await
        .unwrap()
        .stream_connect("a", &b_dest)
        .await
        .unwrap();
    let (from, mut br, mut bw) = accept.peer().await.unwrap();
    assert_eq!(from, a_dest);

    aw.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    br.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");
    bw.write_all(b"pong").await.unwrap();
    ar.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"pong");

    let sessions = bridge.session_history();
    assert_eq!(sessions.len(), 2);
    for s in sessions {
        assert!(
            s.options.contains(&("SIGNATURE_TYPE".into(), "7".into())),
            "{s:?}"
        );
        assert!(
            s.options.contains(&("inbound.quantity".into(), "3".into())),
            "{s:?}"
        );
    }
    assert_eq!(bridge.violations(), Vec::<String>::new());
}

#[tokio::test]
async fn errors_of_the_bridge_are_reported() {
    let bridge = FakeSamBridge::start().await.unwrap();
    let mut a = SamConn::open(bridge.addr()).await.unwrap();
    a.session_create("a", &TUNNELS).await.unwrap();

    // Ids are unique.
    let mut dup = SamConn::open(bridge.addr()).await.unwrap();
    match dup.session_create("a", &TUNNELS).await {
        Err(SamError::Result {
            code: SamResultCode::DuplicatedId,
            ..
        }) => {}
        other => panic!("{other:?}"),
    }
    // Unknown names.
    match a.naming_lookup("nowhere.i2p").await {
        Err(SamError::Result {
            code: SamResultCode::KeyNotFound,
            ..
        }) => {}
        other => panic!("{other:?}"),
    }
    // A destination nobody holds.
    let nobody = super::fake_bridge::random_destination();
    let conn = SamConn::open(bridge.addr()).await.unwrap();
    match conn.stream_connect("a", &nobody).await {
        Err(SamError::Result {
            code: SamResultCode::CantReachPeer,
            ..
        }) => {}
        other => panic!("{:?}", other.map(|_| ())),
    }
    // An unknown session id.
    let conn = SamConn::open(bridge.addr()).await.unwrap();
    match conn.stream_accept("zz").await {
        Err(SamError::Result {
            code: SamResultCode::InvalidId,
            ..
        }) => {}
        other => panic!("{:?}", other.map(|_| ())),
    }
    assert_eq!(bridge.violations(), Vec::<String>::new());
}

#[tokio::test]
async fn version_negotiation_fails_cleanly() {
    let bridge = FakeSamBridge::start().await.unwrap();
    bridge.set_versions("3.2", "3.3");
    match SamConn::open(bridge.addr()).await {
        Err(SamError::Result {
            code: SamResultCode::NoVersion,
            ..
        }) => {}
        other => panic!("{:?}", other.map(|_| ())),
    }
}

#[tokio::test]
async fn bridge_down_drops_sessions_and_comes_back() {
    let bridge = FakeSamBridge::start().await.unwrap();
    let mut a = SamConn::open(bridge.addr()).await.unwrap();
    a.session_create("a", &TUNNELS).await.unwrap();
    bridge.go_down();
    assert!(a.wait_closed().await.to_string().contains("I/O"));
    assert!(SamConn::open(bridge.addr()).await.is_err());
    assert!(bridge.sessions().is_empty());
    bridge.go_up();
    let mut a = SamConn::open(bridge.addr()).await.unwrap();
    a.session_create("a", &TUNNELS).await.unwrap();
    assert_eq!(bridge.sessions().len(), 1);
}
