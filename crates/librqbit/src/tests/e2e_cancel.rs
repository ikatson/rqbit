use std::time::Duration;

use super::fake_peer::{BLOCK, FakePeer, SeederOpts, Wire, peer_id, run, start_seeder, uploaded};

// A request that the peer cancels while it waits for its turn is neither read nor sent nor
// counted as uploaded.
#[tokio::test(flavor = "multi_thread")]
async fn test_cancelled_blocks_are_not_uploaded() -> anyhow::Result<()> {
    run(async {
        // One block per second: the first goes out right away, the others have to wait.
        let seed = start_seeder(SeederOpts {
            upload_bps: Some(BLOCK),
            ..Default::default()
        })
        .await?;
        let mut peer =
            FakePeer::connect(seed.addr(), seed.info_hash, &peer_id(b"-FK0001-")).await?;
        peer.interested().await?;
        let mut batch = Vec::new();
        for (piece, begin) in [(0, 0), (0, BLOCK), (1, 0), (1, BLOCK)] {
            batch.extend(FakePeer::request_frame(piece, begin, BLOCK));
        }
        batch.extend(FakePeer::frame(8, &FakePeer::triple(0, BLOCK, BLOCK)));
        batch.extend(FakePeer::frame(8, &FakePeer::triple(1, 0, BLOCK)));
        peer.send_raw(&batch).await?;

        let pieces = peer.expect_pieces(2, Duration::from_secs(10)).await?;
        let got: Vec<_> = pieces.iter().map(|(p, b, _)| (*p, *b)).collect();
        assert_eq!(got, vec![(0, 0), (1, BLOCK)]);
        assert_eq!(pieces[0].2, seed.block(0, 0, BLOCK));
        assert_eq!(pieces[1].2, seed.block(1, BLOCK, BLOCK));
        let (rest, closed) = peer.collect(Duration::from_millis(1500)).await;
        assert!(!closed);
        assert!(
            rest.iter().all(|m| !matches!(m, Wire::Piece { .. })),
            "cancelled blocks were sent: {rest:?}"
        );
        assert_eq!(uploaded(&seed), 2 * BLOCK as u64);

        // Asking again for a block that was cancelled works.
        peer.request(0, BLOCK, BLOCK).await?;
        let again = peer.expect_pieces(1, Duration::from_secs(10)).await?;
        assert_eq!((again[0].0, again[0].1), (0, BLOCK));
        assert_eq!(uploaded(&seed), 3 * BLOCK as u64);
        Ok(())
    })
    .await
}
