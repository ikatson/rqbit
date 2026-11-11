use std::time::{Duration, Instant};

use anyhow::Context;
use tokio::{net::TcpListener, time::timeout};

use super::fake_peer::{ClientOpts, SeederOpts, run, start_client, start_seeder};
use crate::PeerConnectionOptions;

// Dials the given peers and returns when each of them was reached.
async fn dial_times(connect_rate: Option<u32>, peers: usize) -> anyhow::Result<Vec<Instant>> {
    let seed = start_seeder(SeederOpts::default()).await?;
    let mut initial_peers = Vec::new();
    let mut accepted = Vec::new();
    for _ in 0..peers {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        initial_peers.push(listener.local_addr()?);
        // Keeps the accepted socket open, so that the dial stays in progress.
        accepted.push(tokio::spawn(async move {
            let (conn, _) = listener.accept().await?;
            anyhow::Ok((Instant::now(), conn))
        }));
    }
    let _client = start_client(
        &seed,
        ClientOpts {
            initial_peers,
            peer_opts: Some(PeerConnectionOptions {
                connect_rate,
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await?;
    let mut times = Vec::new();
    let mut conns = Vec::new();
    for a in accepted {
        let (time, conn) = timeout(Duration::from_secs(15), a)
            .await
            .context("a peer was not dialed")???;
        times.push(time);
        conns.push(conn);
    }
    Ok(times)
}

// Without a rate every peer is dialed at once.
#[tokio::test(flavor = "multi_thread")]
async fn test_outgoing_connections_are_not_paced_by_default() -> anyhow::Result<()> {
    run(async {
        let times = dial_times(None, 4).await?;
        let spread = times
            .iter()
            .max()
            .unwrap()
            .duration_since(*times.iter().min().unwrap());
        assert!(spread < Duration::from_millis(1200), "{spread:?}");
        Ok(())
    })
    .await
}

// With connect_rate = 2 a torrent starts a connection every 500 ms.
#[tokio::test(flavor = "multi_thread")]
async fn test_outgoing_connections_are_paced_by_connect_rate() -> anyhow::Result<()> {
    run(async {
        let times = dial_times(Some(2), 4).await?;
        let first = *times.iter().min().unwrap();
        let last = *times.iter().max().unwrap();
        // Three gaps of 500 ms between four dials.
        assert!(
            last.duration_since(first) >= Duration::from_millis(1200),
            "dials were not spread out: {:?}",
            last.duration_since(first)
        );
        Ok(())
    })
    .await
}
