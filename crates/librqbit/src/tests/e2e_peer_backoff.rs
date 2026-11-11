use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::net::TcpListener;

use super::fake_peer::{ClientOpts, SeederOpts, run, start_client, start_seeder};
use crate::PeerBackoffConfig;

// A peer that accepts the connection and closes it right away is dialed again once the backoff
// is over, over and over. How soon is decided by the configured backoff.
async fn count_dials(backoff: Option<PeerBackoffConfig>, watch: Duration) -> anyhow::Result<usize> {
    let seed = start_seeder(SeederOpts::default()).await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let dials = Arc::new(AtomicUsize::new(0));
    let counter = dials.clone();
    let acceptor = tokio::spawn(async move {
        while let Ok((conn, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            drop(conn);
        }
    });
    let _client = start_client(
        &seed,
        ClientOpts {
            initial_peers: vec![addr],
            backoff,
            ..Default::default()
        },
    )
    .await?;
    let deadline = Instant::now() + watch;
    tokio::time::sleep_until(deadline.into()).await;
    acceptor.abort();
    Ok(dials.load(Ordering::SeqCst))
}

#[tokio::test(flavor = "multi_thread")]
async fn test_configured_backoff_redials_a_failing_peer_sooner() -> anyhow::Result<()> {
    run(async {
        let dials = count_dials(
            Some(PeerBackoffConfig {
                min_delay: Duration::from_millis(50),
                factor: 1.,
                max_delay: Duration::from_millis(100),
                total_delay: None,
            }),
            Duration::from_secs(4),
        )
        .await?;
        assert!(dials >= 4, "only {dials} dials");
        Ok(())
    })
    .await
}

// The default backoff waits at least 10 s after the first failure.
#[tokio::test(flavor = "multi_thread")]
async fn test_default_backoff_does_not_redial_within_ten_seconds() -> anyhow::Result<()> {
    run(async {
        let dials = count_dials(None, Duration::from_secs(3)).await?;
        assert_eq!(dials, 1);
        Ok(())
    })
    .await
}
