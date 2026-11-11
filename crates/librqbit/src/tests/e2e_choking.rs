use std::{
    collections::HashSet,
    num::NonZeroU32,
    time::{Duration, Instant},
};

use super::fake_peer::{
    BLOCK, ClientOpts, FakePeer, Fixture, Recv, SeederOpts, Wire, peer_id, run, start_client,
    start_seeder, uploaded,
};
use crate::UploadSlots;

fn fixed(slots: u32) -> UploadSlots {
    UploadSlots::Fixed(NonZeroU32::new(slots).unwrap())
}

async fn connect(seed: &Fixture, id: &[u8; 8]) -> anyhow::Result<FakePeer> {
    FakePeer::connect(seed.addr(), seed.info_hash, &peer_id(id)).await
}

fn no_pieces(msgs: &[Wire]) -> bool {
    msgs.iter().all(|m| !matches!(m, Wire::Piece { .. }))
}

// A connection starts choked. Only the peer with the upload slot is served; what the others ask
// for is ignored, and the slot of a peer that leaves goes to the next one at once.
#[tokio::test(flavor = "multi_thread")]
async fn test_only_peers_with_an_upload_slot_are_served() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts {
            upload_slots: fixed(1),
            ..Default::default()
        })
        .await?;

        let mut a = connect(&seed, b"-FK0001-").await?;
        let (msgs, closed) = a.collect(Duration::from_millis(300)).await;
        assert!(!closed);
        assert!(
            !msgs.contains(&Wire::Unchoke),
            "unchoked without being interested: {msgs:?}"
        );
        a.interested().await?;
        a.expect(Wire::Unchoke, Duration::from_secs(5)).await?;

        let mut b = connect(&seed, b"-FK0002-").await?;
        b.interested().await?;
        b.request(0, 0, BLOCK).await?;
        let (msgs, closed) = b.collect(Duration::from_millis(500)).await;
        assert!(!closed, "a request of a choked peer is ignored, no more");
        assert!(
            !msgs.contains(&Wire::Unchoke) && no_pieces(&msgs),
            "the only slot is taken: {msgs:?}"
        );

        a.request(1, 0, BLOCK).await?;
        a.expect_pieces(1, Duration::from_secs(10)).await?;

        drop(a);
        b.expect(Wire::Unchoke, Duration::from_secs(10)).await?;
        b.request(0, 0, BLOCK).await?;
        let pieces = b.expect_pieces(1, Duration::from_secs(10)).await?;
        assert_eq!((pieces[0].0, pieces[0].1), (0, 0));
        assert_eq!(uploaded(&seed), 2 * BLOCK as u64);
        Ok(())
    })
    .await
}

// A peer that loses interest is choked, what it still had queued is neither sent nor counted,
// and the next peer gets its slot.
#[tokio::test(flavor = "multi_thread")]
async fn test_losing_interest_drops_the_queue_and_frees_the_slot() -> anyhow::Result<()> {
    run(async {
        // One block per second: the first goes out right away, the others have to wait.
        let seed = start_seeder(SeederOpts {
            upload_bps: Some(BLOCK),
            upload_slots: fixed(1),
            ..Default::default()
        })
        .await?;

        let mut a = connect(&seed, b"-FK0001-").await?;
        a.interested().await?;
        a.expect(Wire::Unchoke, Duration::from_secs(5)).await?;
        let mut b = connect(&seed, b"-FK0002-").await?;
        b.interested().await?;

        let mut batch = Vec::new();
        for (piece, begin) in [(0, 0), (0, BLOCK), (1, 0), (1, BLOCK)] {
            batch.extend(FakePeer::request_frame(piece, begin, BLOCK));
        }
        a.send_raw(&batch).await?;
        let pieces = a.expect_pieces(1, Duration::from_secs(10)).await?;
        assert_eq!((pieces[0].0, pieces[0].1), (0, 0));

        a.not_interested().await?;
        a.expect(Wire::Choke, Duration::from_secs(5)).await?;
        b.expect(Wire::Unchoke, Duration::from_secs(5)).await?;
        let (rest, closed) = a.collect(Duration::from_millis(2500)).await;
        assert!(!closed);
        assert!(no_pieces(&rest), "sent after the choke: {rest:?}");
        assert_eq!(uploaded(&seed), BLOCK as u64);

        b.request(1, 0, BLOCK).await?;
        let pieces = b.expect_pieces(1, Duration::from_secs(10)).await?;
        assert_eq!((pieces[0].0, pieces[0].1), (1, 0));
        assert_eq!(uploaded(&seed), 2 * BLOCK as u64);
        Ok(())
    })
    .await
}

// The slots are handed out anew every 10 seconds: the peer that takes data gets the regular slot
// of the one that takes none, and the optimistic slot this frees goes to the peer that waited.
// The waiting peer says nothing for much longer than the read timeout, and is not dropped for it.
#[tokio::test(flavor = "multi_thread")]
async fn test_slots_move_to_the_peers_that_use_them() -> anyhow::Result<()> {
    run(async {
        // A regular slot and an optimistic one.
        let seed = start_seeder(SeederOpts {
            upload_bps: Some(BLOCK),
            upload_slots: fixed(2),
            read_write_timeout: Some(Duration::from_secs(3)),
        })
        .await?;

        let mut a = connect(&seed, b"-FK0001-").await?;
        a.interested().await?;
        a.expect(Wire::Unchoke, Duration::from_secs(5)).await?;
        let mut b = connect(&seed, b"-FK0002-").await?;
        b.interested().await?;
        b.expect(Wire::Unchoke, Duration::from_secs(5)).await?;
        let mut c = connect(&seed, b"-FK0003-").await?;
        c.interested().await?;

        // `a` has the regular slot and only keeps its connection alive. `b` keeps asking, and
        // gets a block per second.
        let deadline = Instant::now() + Duration::from_secs(25);
        let mut b_got = Vec::new();
        let mut asked = 0;
        loop {
            anyhow::ensure!(Instant::now() < deadline, "the idle peer kept its slot");
            a.keep_alive().await?;
            b.request(asked % 4, 0, BLOCK).await?;
            asked += 1;
            b_got.extend(b.collect(Duration::from_millis(50)).await.0);
            match a.recv(Duration::from_millis(500)).await {
                Recv::Msg(Wire::Choke) => break,
                Recv::Closed => anyhow::bail!("the idle peer was disconnected"),
                Recv::Msg(_) | Recv::Timeout => {}
            }
        }
        c.expect(Wire::Unchoke, Duration::from_secs(5)).await?;
        let (more, closed) = b.collect(Duration::from_millis(200)).await;
        b_got.extend(more);
        assert!(!closed);
        assert!(
            !b_got.contains(&Wire::Choke),
            "the peer that takes data keeps a slot"
        );
        Ok(())
    })
    .await
}

// The silence of a peer that waited for a slot is not held against it the moment it gets one:
// it has the read timeout from then on to ask for something.
#[tokio::test(flavor = "multi_thread")]
async fn test_unchoked_peer_is_given_time_to_ask() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts {
            upload_slots: fixed(1),
            read_write_timeout: Some(Duration::from_secs(3)),
            ..Default::default()
        })
        .await?;
        let mut a = connect(&seed, b"-FK0001-").await?;
        a.interested().await?;
        a.expect(Wire::Unchoke, Duration::from_secs(5)).await?;
        let mut b = connect(&seed, b"-FK0002-").await?;
        b.interested().await?;
        let waiting_since = Instant::now();

        // `b` has said nothing for 5.5 s, almost two read timeouts, when the slot becomes
        // its own...
        while waiting_since.elapsed() < Duration::from_millis(5500) {
            a.keep_alive().await?;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        drop(a);
        b.expect(Wire::Unchoke, Duration::from_secs(2)).await?;
        // ... and it asks for its first block a second later, 6.5 s after its last message.
        tokio::time::sleep(Duration::from_millis(6500).saturating_sub(waiting_since.elapsed()))
            .await;
        b.request(0, 0, BLOCK).await?;
        b.expect_pieces(1, Duration::from_secs(5)).await?;
        Ok(())
    })
    .await
}

// A peer that chokes us owes us nothing: the client waits for its turn, for longer than the
// read timeout, instead of hanging up on the peer and dialing it again later.
#[tokio::test(flavor = "multi_thread")]
async fn test_client_waits_for_a_peer_that_chokes_it() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let client = start_client(
            &seed,
            ClientOpts {
                listen: true,
                read_write_timeout: Some(Duration::from_secs(2)),
                ..Default::default()
            },
        )
        .await?;

        let mut peer = connect(&client, b"-FK0001-").await?;
        peer.bitfield(&[0b1111_0000]).await?;
        peer.expect(Wire::Interested, Duration::from_secs(5))
            .await?;
        let (msgs, closed) = peer.collect(Duration::from_secs(5)).await;
        assert!(!closed, "hung up on a peer that had not unchoked it yet");
        assert!(
            msgs.iter().all(|m| !matches!(m, Wire::Request(..))),
            "asked while choked: {msgs:?}"
        );

        peer.unchoke().await?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match peer.recv(left).await {
                Recv::Msg(Wire::Request(..)) => break,
                Recv::Msg(_) => {}
                Recv::Timeout => anyhow::bail!("nothing asked for after the unchoke"),
                Recv::Closed => anyhow::bail!("closed after the unchoke"),
            }
        }
        Ok(())
    })
    .await
}

// A choke voids what the client asked the peer for (BEP 3). The client does not wait for those
// blocks (and then give up on the peer when they don't come): it stays, and asks again once it
// is unchoked.
#[tokio::test(flavor = "multi_thread")]
async fn test_client_asks_again_for_what_a_choke_dropped() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let client = start_client(
            &seed,
            ClientOpts {
                listen: true,
                read_write_timeout: Some(Duration::from_secs(2)),
                ..Default::default()
            },
        )
        .await?;

        let mut peer = connect(&client, b"-FK0001-").await?;
        peer.bitfield(&[0b1111_0000]).await?;
        peer.unchoke().await?;

        // The client asks for all 8 blocks of the torrent. It gets one, and a choke.
        let mut asked = Vec::new();
        while asked.len() < 8 {
            match peer.recv(Duration::from_secs(10)).await {
                Recv::Msg(Wire::Request(piece, begin, len)) => asked.push((piece, begin, len)),
                Recv::Msg(_) => {}
                Recv::Timeout | Recv::Closed => anyhow::bail!("asked for {asked:?} only"),
            }
        }
        let (piece, begin, len) = asked[0];
        peer.piece(piece, begin, seed.block(piece, begin, len))
            .await?;
        peer.choke().await?;

        let (msgs, closed) = peer.collect(Duration::from_secs(5)).await;
        assert!(!closed, "gave up on a peer that owed it nothing");
        assert!(
            msgs.iter().all(|m| !matches!(m, Wire::Request(..))),
            "asked while choked: {msgs:?}"
        );

        // Unchoked, it asks for what the choke dropped, and completes the torrent with it.
        peer.unchoke().await?;
        let mut again = HashSet::new();
        let done = client.handle.wait_until_completed();
        tokio::pin!(done);
        loop {
            tokio::select! {
                r = &mut done => {
                    r?;
                    break;
                }
                m = peer.recv(Duration::from_secs(20)) => match m {
                    Recv::Msg(Wire::Request(piece, begin, len)) => {
                        again.insert((piece, begin, len));
                        peer.piece(piece, begin, seed.block(piece, begin, len)).await?;
                    }
                    Recv::Msg(_) => {}
                    Recv::Timeout => anyhow::bail!("not finished, asked again for {again:?}"),
                    Recv::Closed => anyhow::bail!("closed, asked again for {again:?}"),
                },
            }
        }
        for dropped in &asked[1..] {
            assert!(
                again.contains(dropped),
                "{dropped:?} was not asked for again"
            );
        }
        Ok(())
    })
    .await
}
