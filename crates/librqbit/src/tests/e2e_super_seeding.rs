//! Super seeding (BEP 16) end to end: a super seeding session on one side, wire-level fake peers
//! or other sessions on the other.

use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

use anyhow::{Context, bail, ensure};
use tokio::time::timeout;

use super::fake_peer::{
    BLOCK, ClientOpts, FakePeer, Fixture, PIECE_LEN, PIECES, Recv, SeederExt, SeederOpts, Wire,
    peer_id, run, start_client, start_seeder, start_seeder_ext, uploaded,
};
use crate::tests::test_util::wait_until;

/// How long any single step may take.
const STEP: Duration = Duration::from_secs(10);

/// How long a connection is watched to see that nothing arrives on it.
const QUIET: Duration = Duration::from_millis(700);

/// The default wait for the piece set of a peer.
const PIECE_SET_WAIT: Duration = Duration::from_secs(2);

async fn super_seeder(pieces: Option<u32>) -> anyhow::Result<Fixture> {
    start_seeder_ext(
        SeederOpts::default(),
        SeederExt {
            pieces,
            super_seeding: true,
            ..Default::default()
        },
    )
    .await
}

/// Shortens the bounded waits of the live torrent of `f`.
fn set_timeouts(f: &Fixture, reveal: Duration, piece_set: Duration) -> anyhow::Result<()> {
    f.handle
        .live()
        .context("the torrent is not live")?
        .super_seeding
        .set_timeouts(reveal, piece_set);
    Ok(())
}

/// A Fast connection to `f` that told it has nothing. Checks that `f` pretends the same.
async fn fast_peer(f: &Fixture, id: &[u8; 8]) -> anyhow::Result<FakePeer> {
    let mut peer = FakePeer::connect_fast(f.addr(), f.info_hash, &peer_id(id)).await?;
    peer.have_none().await?;
    let first = peer.first_message(STEP).await?;
    ensure!(
        first == Wire::HaveNone,
        "the super seeder opened with {first:?}"
    );
    Ok(peer)
}

/// Whether `m` tells the peer about more than one piece at a time.
fn advertises(m: &Wire) -> bool {
    matches!(m, Wire::HaveAll | Wire::Bitfield(_) | Wire::AllowedFast(_))
}

/// Waits for the next `Have`, skipping other messages. Fails on anything that shows the peer
/// more than one piece.
async fn next_have(peer: &mut FakePeer, within: Duration) -> anyhow::Result<u32> {
    let deadline = Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match peer.recv(left).await {
            Recv::Msg(Wire::Have(piece)) => return Ok(piece),
            Recv::Msg(m) if advertises(&m) => bail!("the super seeder sent {m:?}"),
            Recv::Msg(_) => {}
            Recv::Timeout => bail!("no Have in {within:?}"),
            Recv::Closed => bail!("closed before a Have"),
        }
    }
}

/// Checks that no piece is announced for a while, and that the connection stays up.
async fn expect_no_have(peer: &mut FakePeer) -> anyhow::Result<()> {
    let (msgs, closed) = peer.collect(QUIET).await;
    ensure!(!closed, "closed, after {msgs:?}");
    ensure!(
        !msgs
            .iter()
            .any(|m| matches!(m, Wire::Have(_)) || advertises(m)),
        "announced {msgs:?}"
    );
    Ok(())
}

/// Says interested and waits for the unchoke.
async fn unchoked(peer: &mut FakePeer) -> anyhow::Result<()> {
    peer.interested().await?;
    peer.expect(Wire::Unchoke, STEP).await
}

/// Downloads every block of `piece` and checks the data against `seed`.
async fn download(peer: &mut FakePeer, seed: &Fixture, piece: u32) -> anyhow::Result<()> {
    for begin in (0..PIECE_LEN).step_by(BLOCK as usize) {
        peer.request(piece, begin, BLOCK).await?;
    }
    let blocks = peer
        .expect_pieces((PIECE_LEN / BLOCK) as usize, STEP)
        .await?;
    for (index, begin, data) in blocks {
        ensure!(index == piece, "got piece {index} instead of {piece}");
        ensure!(
            data == seed.block(piece, begin, BLOCK),
            "wrong data in piece {piece} at {begin}"
        );
    }
    Ok(())
}

async fn wait_live_and_finished(f: &Fixture) -> anyhow::Result<()> {
    wait_until(
        || {
            ensure!(f.handle.live().is_some(), "not live");
            ensure!(f.handle.stats().finished, "not finished");
            Ok(())
        },
        STEP,
    )
    .await
}

// A Fast connection gets HaveNone and then a single Have, and no allowed-fast set even though it
// said it has nothing. A request for another piece is rejected, the connection stays, and the
// piece shown is served.
#[tokio::test(flavor = "multi_thread")]
async fn test_fast_peer_is_shown_one_piece() -> anyhow::Result<()> {
    run(async {
        let seed = super_seeder(None).await?;
        let mut peer = fast_peer(&seed, b"-SS0001-").await?;
        let shown = next_have(&mut peer, STEP).await?;
        expect_no_have(&mut peer).await?;

        unchoked(&mut peer).await?;
        let hidden = (shown + 1) % PIECES;
        peer.request(hidden, 0, BLOCK).await?;
        peer.expect(Wire::RejectRequest(hidden, 0, BLOCK), STEP)
            .await?;
        download(&mut peer, &seed, shown).await
    })
    .await
}

// A plain connection gets no bitfield, only one Have, even if the peer sends no piece set of its
// own. A request for another piece is ignored and the connection stays.
#[tokio::test(flavor = "multi_thread")]
async fn test_plain_peer_gets_no_bitfield() -> anyhow::Result<()> {
    run(async {
        let seed = super_seeder(None).await?;
        set_timeouts(&seed, Duration::from_secs(30), Duration::from_millis(100))?;
        let mut peer =
            FakePeer::connect(seed.addr(), seed.info_hash, &peer_id(b"-SS0002-")).await?;
        let first = peer.first_message(STEP).await?;
        let Wire::Have(shown) = first else {
            bail!("the super seeder opened with {first:?}");
        };
        expect_no_have(&mut peer).await?;

        unchoked(&mut peer).await?;
        let hidden = (shown + 1) % PIECES;
        peer.request(hidden, 0, BLOCK).await?;
        let (msgs, closed) = peer.collect(QUIET).await;
        ensure!(!closed, "closed, after {msgs:?}");
        ensure!(
            !msgs
                .iter()
                .any(|m| matches!(m, Wire::Piece { .. } | Wire::RejectRequest(..))),
            "answered a request for a hidden piece: {msgs:?}"
        );
        download(&mut peer, &seed, shown).await
    })
    .await
}

// Two peers are shown different pieces. The next piece is shown to a peer only once its last one
// showed up at the other peer.
#[tokio::test(flavor = "multi_thread")]
async fn test_next_piece_waits_until_the_last_one_spreads() -> anyhow::Result<()> {
    run(async {
        let seed = super_seeder(None).await?;
        let mut a = fast_peer(&seed, b"-SS000A-").await?;
        let a_first = next_have(&mut a, STEP).await?;
        let mut b = fast_peer(&seed, b"-SS000B-").await?;
        let b_first = next_have(&mut b, STEP).await?;
        ensure!(a_first != b_first, "both peers were shown {a_first}");

        unchoked(&mut a).await?;
        download(&mut a, &seed, a_first).await?;
        // B lacks the piece and did not announce it: it did not spread.
        expect_no_have(&mut a).await?;

        b.have(a_first).await?;
        let a_second = next_have(&mut a, STEP).await?;
        ensure!(
            ![a_first, b_first].contains(&a_second),
            "shown {a_second} after {a_first}, while B was shown {b_first}"
        );
        Ok(())
    })
    .await
}

// A peer that is alone is shown the next piece as soon as it has the last one, until it has
// seen them all.
#[tokio::test(flavor = "multi_thread")]
async fn test_lone_peer_is_shown_every_piece_in_turn() -> anyhow::Result<()> {
    run(async {
        let seed = super_seeder(None).await?;
        let mut peer = fast_peer(&seed, b"-SS0003-").await?;
        let mut piece = next_have(&mut peer, STEP).await?;
        unchoked(&mut peer).await?;
        let mut shown = vec![piece];
        loop {
            download(&mut peer, &seed, piece).await?;
            if shown.len() == PIECES as usize {
                break;
            }
            piece = next_have(&mut peer, STEP).await?;
            shown.push(piece);
        }
        expect_no_have(&mut peer).await?;
        let distinct: BTreeSet<u32> = shown.iter().copied().collect();
        assert_eq!(distinct, (0..PIECES).collect(), "shown in turn: {shown:?}");
        Ok(())
    })
    .await
}

// A piece that does not spread holds the next one back only for the bounded wait.
#[tokio::test(flavor = "multi_thread")]
async fn test_unspread_piece_is_followed_after_the_bounded_wait() -> anyhow::Result<()> {
    run(async {
        let seed = super_seeder(None).await?;
        let wait = Duration::from_millis(300);
        set_timeouts(&seed, wait, PIECE_SET_WAIT)?;
        let mut a = fast_peer(&seed, b"-SS000A-").await?;
        let first = next_have(&mut a, STEP).await?;
        // B lacks every piece and never announces one.
        let mut b = fast_peer(&seed, b"-SS000B-").await?;
        next_have(&mut b, STEP).await?;

        unchoked(&mut a).await?;
        let asked = Instant::now();
        download(&mut a, &seed, first).await?;
        let second = next_have(&mut a, STEP).await?;
        ensure!(second != first, "shown {first} twice");
        ensure!(
            asked.elapsed() >= wait,
            "the next piece came {:?} after asking for the first",
            asked.elapsed()
        );
        Ok(())
    })
    .await
}

// The piece shown is the rarest one the peer lacks: never one it has, and preferably one the other
// peer lacks too.
#[tokio::test(flavor = "multi_thread")]
async fn test_rarest_piece_the_peer_lacks_is_shown() -> anyhow::Result<()> {
    run(async {
        let pieces = 8;
        let seed = super_seeder(Some(pieces)).await?;
        set_timeouts(&seed, Duration::from_millis(100), PIECE_SET_WAIT)?;

        // B has pieces 0 to 5.
        let mut b =
            FakePeer::connect_fast(seed.addr(), seed.info_hash, &peer_id(b"-SS000B-")).await?;
        b.bitfield(&[0b1111_1100]).await?;
        let first = b.first_message(STEP).await?;
        ensure!(
            first == Wire::HaveNone,
            "the super seeder opened with {first:?}"
        );
        let b_shown = next_have(&mut b, STEP).await?;
        ensure!(
            matches!(b_shown, 6 | 7),
            "B was shown {b_shown}, which it has"
        );

        // A has piece 0. Pieces 6 and 7 are the rarest, and one of them is shown to B already.
        let mut a =
            FakePeer::connect_fast(seed.addr(), seed.info_hash, &peer_id(b"-SS000A-")).await?;
        a.bitfield(&[0b1000_0000]).await?;
        let first = a.first_message(STEP).await?;
        ensure!(
            first == Wire::HaveNone,
            "the super seeder opened with {first:?}"
        );
        let a_shown = next_have(&mut a, STEP).await?;
        ensure!(
            matches!(a_shown, 6 | 7) && a_shown != b_shown,
            "A was shown {a_shown}, B {b_shown}"
        );

        // Alone, A is shown every piece it lacks in turn, never piece 0.
        drop(b);
        unchoked(&mut a).await?;
        let mut shown = vec![a_shown];
        let mut piece = a_shown;
        loop {
            download(&mut a, &seed, piece).await?;
            if shown.len() == pieces as usize - 1 {
                break;
            }
            piece = next_have(&mut a, STEP).await?;
            shown.push(piece);
        }
        expect_no_have(&mut a).await?;
        let distinct: BTreeSet<u32> = shown.iter().copied().collect();
        assert_eq!(distinct, (1..pieces).collect(), "shown in turn: {shown:?}");
        Ok(())
    })
    .await
}

// Turning the flag off tells every connection about the pieces it was not shown, once each, and
// serves them.
#[tokio::test(flavor = "multi_thread")]
async fn test_turning_the_flag_off_shows_every_piece() -> anyhow::Result<()> {
    run(async {
        let seed = super_seeder(None).await?;
        let mut peer = fast_peer(&seed, b"-SS0004-").await?;
        let shown = next_have(&mut peer, STEP).await?;

        seed.session.set_super_seeding(&seed.handle, false).await?;
        assert!(!seed.handle.super_seeding());
        let mut known = BTreeSet::from([shown]);
        while known.len() < PIECES as usize {
            let piece = next_have(&mut peer, STEP).await?;
            ensure!(known.insert(piece), "piece {piece} announced twice");
        }
        expect_no_have(&mut peer).await?;

        unchoked(&mut peer).await?;
        download(&mut peer, &seed, (shown + 1) % PIECES).await
    })
    .await
}

// Turning the flag on for a live seeder changes what new connections see. A connection made
// before keeps seeing everything.
#[tokio::test(flavor = "multi_thread")]
async fn test_turning_the_flag_on_applies_to_new_connections() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        assert!(!seed.handle.super_seeding());
        let mut old =
            FakePeer::connect_fast(seed.addr(), seed.info_hash, &peer_id(b"-SS0005-")).await?;
        old.have_none().await?;
        let first = old.first_message(STEP).await?;
        ensure!(first == Wire::HaveAll, "a seeder opened with {first:?}");

        seed.session.set_super_seeding(&seed.handle, true).await?;
        assert!(seed.handle.super_seeding());
        let stats = seed.handle.stats();
        assert!(stats.super_seeding);
        assert_eq!(serde_json::to_value(&stats)?["super_seeding"], true);

        let mut new = fast_peer(&seed, b"-SS0006-").await?;
        let shown = next_have(&mut new, STEP).await?;
        expect_no_have(&mut new).await?;

        unchoked(&mut old).await?;
        download(&mut old, &seed, (shown + 1) % PIECES).await
    })
    .await
}

// A leecher added with the flag downloads as usual, and super seeds to the connections made
// once it has the whole torrent.
#[tokio::test(flavor = "multi_thread")]
async fn test_leecher_super_seeds_once_complete() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let client = start_client(
            &seed,
            ClientOpts {
                listen: true,
                initial_peers: vec![seed.addr()],
                super_seeding: true,
                ..Default::default()
            },
        )
        .await?;
        assert!(client.handle.super_seeding());
        assert!(client.handle.stats().super_seeding);
        timeout(STEP, client.handle.wait_until_completed())
            .await
            .context("the leecher did not complete")??;

        let mut peer = fast_peer(&client, b"-SS0007-").await?;
        let shown = next_have(&mut peer, STEP).await?;
        expect_no_have(&mut peer).await?;
        unchoked(&mut peer).await?;
        download(&mut peer, &seed, shown).await
    })
    .await
}

// The flag lives with the torrent, not with its live state: a pause drops the live state and
// the resume builds a new one, and the connections made afterwards are still super seeded.
#[tokio::test(flavor = "multi_thread")]
async fn test_super_seeding_survives_pause_and_resume() -> anyhow::Result<()> {
    run(async {
        let seed = super_seeder(None).await?;
        let before = seed.handle.live().context("not live")?;
        seed.session.pause(&seed.handle).await?;
        seed.session.unpause(&seed.handle).await?;
        wait_until(
            || {
                let live = seed.handle.live().context("not live")?;
                ensure!(!std::sync::Arc::ptr_eq(&live, &before), "not resumed yet");
                Ok(())
            },
            STEP,
        )
        .await?;
        wait_live_and_finished(&seed).await?;
        assert!(seed.handle.super_seeding());

        let mut peer = fast_peer(&seed, b"-SS0008-").await?;
        let shown = next_have(&mut peer, STEP).await?;
        expect_no_have(&mut peer).await?;
        unchoked(&mut peer).await?;
        download(&mut peer, &seed, shown).await
    })
    .await
}

// The flag set at run time is persisted and restored with the session.
#[tokio::test(flavor = "multi_thread")]
async fn test_flag_set_at_run_time_survives_a_restart() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let client = start_client(
            &seed,
            ClientOpts {
                persistence: true,
                ..Default::default()
            },
        )
        .await?;
        assert!(!client.handle.super_seeding());
        client
            .session
            .set_super_seeding(&client.handle, true)
            .await?;
        client.session.stop().await;
        let Fixture { _dir: dir, .. } = client;

        // The new session restores the torrent from the persistence in the folder; adding it
        // again, without the flag, returns that one.
        let client = start_client(
            &seed,
            ClientOpts {
                persistence: true,
                dir: Some(dir),
                ..Default::default()
            },
        )
        .await?;
        assert!(client.handle.super_seeding(), "the flag was not restored");
        Ok(())
    })
    .await
}

// A client downloading from a super seeder asks for each piece as soon as it is shown, and
// announces it as soon as it has it, so the seeder never waits on it.
#[tokio::test(flavor = "multi_thread")]
async fn test_client_follows_a_super_seeder_promptly() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let client = start_client(
            &seed,
            ClientOpts {
                listen: true,
                ..Default::default()
            },
        )
        .await?;
        let mut peer =
            FakePeer::connect_fast(client.addr(), client.info_hash, &peer_id(b"-SS0009-")).await?;
        peer.have_none().await?;
        peer.unchoke().await?;
        // A client that looks at the peer again only when its idle timer (5 seconds) fires
        // spends that long on every piece but the first.
        timeout(Duration::from_secs(10), async {
            for piece in 0..PIECES {
                peer.have(piece).await?;
                let mut served = 0;
                while served < PIECE_LEN / BLOCK {
                    match peer.recv(STEP).await {
                        Recv::Msg(Wire::Request(index, begin, len)) => {
                            ensure!(index == piece, "asked for {index} while shown {piece}");
                            peer.piece(index, begin, seed.block(index, begin, len))
                                .await?;
                            served += 1;
                        }
                        Recv::Msg(_) => {}
                        Recv::Timeout => bail!("piece {piece} was not asked for"),
                        Recv::Closed => bail!("closed while shown piece {piece}"),
                    }
                }
                // Once the client has everything it hangs up on a peer that has everything too,
                // which may come before the last `Have`.
                if piece + 1 < PIECES {
                    peer.expect(Wire::Have(piece), STEP).await?;
                }
            }
            anyhow::Ok(())
        })
        .await
        .context("the client did not follow the super seeder promptly")??;
        timeout(STEP, client.handle.wait_until_completed())
            .await
            .context("the client did not complete")??;
        Ok(())
    })
    .await
}

// Two leecher sessions get every piece from a rate limited super seeder about once: they pass
// the pieces on to each other instead.
#[tokio::test(flavor = "multi_thread")]
async fn test_leechers_trade_and_the_seeder_uploads_about_one_copy() -> anyhow::Result<()> {
    run(async {
        let pieces = 32;
        let seed = start_seeder_ext(
            SeederOpts {
                upload_bps: Some(256 * 1024),
                ..Default::default()
            },
            SeederExt {
                pieces: Some(pieces),
                super_seeding: true,
                ..Default::default()
            },
        )
        .await?;
        let total = u64::from(pieces * PIECE_LEN);
        let a = start_client(
            &seed,
            ClientOpts {
                listen: true,
                initial_peers: vec![seed.addr()],
                ..Default::default()
            },
        )
        .await?;
        let b = start_client(
            &seed,
            ClientOpts {
                listen: true,
                initial_peers: vec![seed.addr(), a.addr()],
                ..Default::default()
            },
        )
        .await?;
        for (name, c) in [("a", &a), ("b", &b)] {
            timeout(Duration::from_secs(45), c.handle.wait_until_completed())
                .await
                .with_context(|| format!("leecher {name} did not complete"))??;
        }
        let up = uploaded(&seed);
        ensure!(up >= total, "the seeder uploaded {up} bytes of {total}");
        ensure!(
            up < total * 3 / 2,
            "the super seeder uploaded {up} bytes for a torrent of {total}"
        );
        Ok(())
    })
    .await
}
