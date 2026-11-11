use std::{net::Ipv4Addr, sync::Arc, time::Duration};

use anyhow::bail;
use tokio::net::TcpStream;

use crate::{
    ListenerMode, Session, SessionOptions,
    listen::ListenerOptions,
    tests::test_util::{setup_test_logging, wait_until},
};

// A peer that connects and then stays silent must not keep a dropped session
// alive until the handshake read times out (10 seconds by default).
#[tokio::test(flavor = "multi_thread")]
async fn test_dropped_session_not_kept_alive_by_silent_incoming_connection() {
    setup_test_logging();

    let session = Session::new_with_opts(
        std::env::temp_dir().join("does_not_exist"),
        SessionOptions {
            dht: None,
            listen: Some(ListenerOptions {
                mode: ListenerMode::TcpOnly,
                listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
                ..Default::default()
            }),
            disable_local_service_discovery: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let listen_addr = session.listen_addr().unwrap();

    // Connect and send nothing: the session is now waiting for our handshake.
    let _silent_peer = TcpStream::connect(listen_addr).await.unwrap();
    // Give the listener time to accept the connection.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let weak = Arc::downgrade(&session);
    drop(session);

    let metrics = tokio::runtime::Handle::current().metrics();
    wait_until(
        || {
            if weak.upgrade().is_some() {
                bail!("session is still alive")
            }
            let num_alive = metrics.num_alive_tasks();
            if num_alive != 0 {
                bail!("metrics.num_alive_tasks() = {num_alive}, expected 0")
            }
            Ok(())
        },
        // Well below the 10 second handshake timeout.
        Duration::from_secs(3),
    )
    .await
    .unwrap();
}
