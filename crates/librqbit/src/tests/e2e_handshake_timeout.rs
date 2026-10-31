use std::time::{Duration, Instant};

use anyhow::Context;
use tokio::{io::AsyncReadExt, net::TcpListener, time::timeout};

use super::fake_peer::{ClientOpts, FakePeer, SeederOpts, run, start_client, start_seeder};
use crate::PeerConnectionOptions;

// Long regular timeout, short handshake timeout: only the latter can end a silent connection
// within the time the tests wait.
fn short_handshake() -> PeerConnectionOptions {
    PeerConnectionOptions {
        read_write_timeout: Some(Duration::from_secs(120)),
        handshake_timeout: Some(Duration::from_secs(1)),
        ..Default::default()
    }
}

// A peer that connects to us and never sends its handshake is dropped after the handshake
// timeout.
#[tokio::test(flavor = "multi_thread")]
async fn test_silent_incoming_peer_is_dropped_after_the_handshake_timeout() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts {
            peer_opts: Some(short_handshake()),
            ..Default::default()
        })
        .await?;
        let mut peer = FakePeer::connect_without_handshake(seed.addr()).await?;
        let started = Instant::now();
        peer.expect_closed(Duration::from_secs(15)).await?;
        let took = started.elapsed();
        assert!(
            took >= Duration::from_millis(800),
            "closed too early: {took:?}"
        );
        Ok(())
    })
    .await
}

// The same for a peer of a live torrent that we dial, whose torrent got the timeout from the
// session's defaults.
#[tokio::test(flavor = "multi_thread")]
async fn test_silent_outgoing_peer_is_dropped_after_the_handshake_timeout() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let silent = TcpListener::bind("127.0.0.1:0").await?;
        let silent_addr = silent.local_addr()?;
        let _client = start_client(
            &seed,
            ClientOpts {
                initial_peers: vec![silent_addr],
                peer_opts: Some(short_handshake()),
                ..Default::default()
            },
        )
        .await?;

        let (mut conn, _) = timeout(Duration::from_secs(10), silent.accept())
            .await
            .context("the client did not dial the peer")??;
        let started = Instant::now();
        // The client's own handshake arrives first, then nothing until it gives up and closes.
        let mut buf = [0u8; 256];
        let closed = timeout(Duration::from_secs(15), async {
            loop {
                match conn.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        })
        .await;
        assert!(closed.is_ok(), "the client kept waiting for a handshake");
        let took = started.elapsed();
        assert!(
            took >= Duration::from_millis(800),
            "closed too early: {took:?}"
        );
        Ok(())
    })
    .await
}
