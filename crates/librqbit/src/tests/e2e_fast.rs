//! The Fast Extension (BEP 6) end to end: a real session on one side, a wire-level fake peer
//! (or a second session) on the other.

use std::{
    collections::HashSet,
    net::Ipv4Addr,
    num::NonZeroU32,
    time::{Duration, Instant},
};

use anyhow::{Context, bail};

use super::fake_peer::{
    BLOCK, ClientOpts, FakePeer, Fixture, PIECE_LEN, Recv, SeederExt, SeederOpts, Wire, peer_id,
    run, start_client, start_seeder, start_seeder_ext,
};
use crate::{
    UploadSlots,
    fast::{ALLOWED_FAST_K, MAX_REJECT_STREAK, MAX_REJECTS_PER_CHOKE, allowed_fast_set},
    tests::test_util::wait_until,
    torrent_state::peer::stats::snapshot::PeerStatsFilter,
};

/// How long any single step may take.
const STEP: Duration = Duration::from_secs(10);

/// Blocks in the default torrent of the fixtures: 4 pieces of 2 blocks.
const BLOCKS: usize = 8;

type Block = (u32, u32, u32);

async fn connect_fast(f: &Fixture, id: &[u8; 8]) -> anyhow::Result<FakePeer> {
    FakePeer::connect_fast(f.addr(), f.info_hash, &peer_id(id)).await
}

async fn connect_plain(f: &Fixture, id: &[u8; 8]) -> anyhow::Result<FakePeer> {
    FakePeer::connect(f.addr(), f.info_hash, &peer_id(id)).await
}

fn fixed(slots: u32) -> UploadSlots {
    UploadSlots::Fixed(NonZeroU32::new(slots).unwrap())
}

fn progress(f: &Fixture) -> u64 {
    f.handle.stats().progress_bytes
}

/// Waits for `n` requests, skipping everything else.
async fn requests(peer: &mut FakePeer, n: usize) -> anyhow::Result<Vec<Block>> {
    let deadline = Instant::now() + STEP;
    let mut out = Vec::new();
    while out.len() < n {
        let left = deadline.saturating_duration_since(Instant::now());
        match peer.recv(left).await {
            Recv::Msg(Wire::Request(piece, begin, len)) => out.push((piece, begin, len)),
            Recv::Msg(_) => {}
            Recv::Timeout => bail!("asked for {out:?} only"),
            Recv::Closed => bail!("closed after asking for {out:?}"),
        }
    }
    Ok(out)
}

/// Serves what the client asks `peer` for until the client has the whole torrent. Returns the
/// blocks it asked for.
async fn serve_until_complete(
    peer: &mut FakePeer,
    seed: &Fixture,
    client: &Fixture,
) -> anyhow::Result<HashSet<Block>> {
    let mut asked = HashSet::new();
    let done = client.handle.wait_until_completed();
    tokio::pin!(done);
    loop {
        tokio::select! {
            r = &mut done => {
                r?;
                return Ok(asked);
            }
            m = peer.recv(Duration::from_secs(20)) => match m {
                Recv::Msg(Wire::Request(piece, begin, len)) => {
                    asked.insert((piece, begin, len));
                    peer.piece(piece, begin, seed.block(piece, begin, len)).await?;
                }
                Recv::Msg(_) => {}
                Recv::Timeout => bail!("not finished, asked for {asked:?}"),
                Recv::Closed => bail!("closed, asked for {asked:?}"),
            },
        }
    }
}

/// The client's view of its live peers: whether each negotiated the Fast Extension.
fn live_fast_flags(f: &Fixture) -> Vec<bool> {
    f.handle.live().map_or_else(Vec::new, |live| {
        live.per_peer_stats_snapshot(PeerStatsFilter::default())
            .peers
            .values()
            .map(|p| p.fast_extension)
            .collect()
    })
}

// ---------------------------------------------------------------------------------------------
// The first message
// ---------------------------------------------------------------------------------------------

// With the Fast Extension the first message is HaveAll from a seeder, HaveNone from a client
// that has nothing, and a Bitfield from one that has some pieces.
#[tokio::test(flavor = "multi_thread")]
async fn test_fast_first_message_is_the_piece_set() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let mut peer = connect_fast(&seed, b"-FK0001-").await?;
        assert_eq!(peer.first_message(STEP).await?, Wire::HaveAll);

        let client = start_client(
            &seed,
            ClientOpts {
                listen: true,
                ..Default::default()
            },
        )
        .await?;
        let mut peer = connect_fast(&client, b"-FK0002-").await?;
        assert_eq!(peer.first_message(STEP).await?, Wire::HaveNone);

        // A peer that has piece 0 only gives it to the client.
        let mut source = connect_fast(&client, b"-FK0003-").await?;
        source.bitfield(&[0b1000_0000]).await?;
        source.unchoke().await?;
        for (piece, begin, len) in requests(&mut source, 2).await? {
            source
                .piece(piece, begin, seed.block(piece, begin, len))
                .await?;
        }
        wait_until(
            || {
                anyhow::ensure!(progress(&client) == u64::from(PIECE_LEN));
                Ok(())
            },
            STEP,
        )
        .await?;
        let mut peer = connect_fast(&client, b"-FK0004-").await?;
        assert_eq!(
            peer.first_message(STEP).await?,
            Wire::Bitfield(vec![0b1000_0000])
        );
        Ok(())
    })
    .await
}

// Without the Fast Extension bit in the peer's handshake nothing changes: a seeder sends its
// bitfield, a client that has nothing sends nothing.
#[tokio::test(flavor = "multi_thread")]
async fn test_fast_first_message_without_the_bit_is_unchanged() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let (mut peer, reserved) = FakePeer::connect_with_reserved(
            seed.addr(),
            seed.info_hash,
            &peer_id(b"-FK0001-"),
            [0, 0, 0, 0, 0, 0x10, 0, 0],
        )
        .await?;
        assert_eq!(reserved[7] & 0x04, 0x04, "our handshake advertises Fast");
        assert_eq!(
            peer.first_message(STEP).await?,
            Wire::Bitfield(vec![0b1111_0000])
        );

        let client = start_client(
            &seed,
            ClientOpts {
                listen: true,
                ..Default::default()
            },
        )
        .await?;
        let mut peer = connect_plain(&client, b"-FK0002-").await?;
        // The peer gets a piece: the client becomes interested. Whatever it would send before
        // that, a HaveNone or an empty bitfield, would arrive first.
        peer.have(0).await?;
        assert_eq!(peer.first_message(STEP).await?, Wire::Interested);
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Rejects we send
// ---------------------------------------------------------------------------------------------

// A choke rejects the requests that wait in the queue, one by one, after the Choke message; what
// was served is not rejected, and nothing is both.
#[tokio::test(flavor = "multi_thread")]
async fn test_fast_choke_rejects_queued_requests() -> anyhow::Result<()> {
    run(async {
        // One block per second: the first goes out right away, the others wait in the queue.
        let seed = start_seeder_ext(
            SeederOpts {
                upload_bps: Some(BLOCK),
                upload_slots: fixed(1),
                ..Default::default()
            },
            SeederExt {
                pieces: Some(64),
                ..Default::default()
            },
        )
        .await?;
        let mut peer = connect_fast(&seed, b"-FK0001-").await?;
        // Not a newcomer: no allowed-fast set, so no request is exempt from the choke.
        peer.bitfield(&[0xff, 0xff, 0, 0, 0, 0, 0, 0]).await?;
        peer.interested().await?;
        peer.expect(Wire::Unchoke, STEP).await?;

        let asked: Vec<Block> = (16..19)
            .flat_map(|piece| [(piece, 0, BLOCK), (piece, BLOCK, BLOCK)])
            .collect();
        for &(piece, begin, len) in &asked {
            peer.request(piece, begin, len).await?;
        }
        // Not interested any more: the peer loses its slot, and is choked. Interested again, it
        // gets the slot back. A block asked for then comes after every answer to the earlier
        // ones: the upload queue is first in, first out. Every message is looked at on the way,
        // the answers come before, between and after the choke and the unchoke.
        peer.not_interested().await?;
        let mut choked = false;
        let mut served = Vec::new();
        let mut rejected = Vec::new();
        loop {
            match peer.recv(STEP).await {
                Recv::Msg(Wire::Choke) if !choked => {
                    choked = true;
                    peer.interested().await?;
                }
                Recv::Msg(Wire::Unchoke) if choked => peer.request(20, 0, BLOCK).await?,
                Recv::Msg(Wire::Piece { index: 20, .. }) => break,
                Recv::Msg(Wire::Piece { index, begin, data }) => {
                    served.push((index, begin, u32::try_from(data.len())?))
                }
                Recv::Msg(Wire::RejectRequest(piece, begin, len)) => {
                    assert!(choked, "rejected {piece} {begin} {len} before the choke");
                    rejected.push((piece, begin, len))
                }
                Recv::Msg(_) => {}
                Recv::Timeout | Recv::Closed => {
                    bail!("no answer to the last request: served {served:?}, rejected {rejected:?}")
                }
            }
        }
        for block in &asked {
            let answers = served
                .iter()
                .chain(&rejected)
                .filter(|b| *b == block)
                .count();
            assert_eq!(
                answers, 1,
                "{block:?}: served {served:?}, rejected {rejected:?}"
            );
        }
        assert!(rejected.len() >= 4, "rejected {rejected:?}");
        Ok(())
    })
    .await
}

// A request from a choked peer is rejected. A peer that keeps asking while choked gets rejects
// up to a limit, then its requests are dropped without an answer, and it stays connected.
#[tokio::test(flavor = "multi_thread")]
async fn test_fast_requests_while_choked_are_rejected_up_to_a_limit() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder_ext(
            SeederOpts::default(),
            SeederExt {
                pieces: Some(64),
                ..Default::default()
            },
        )
        .await?;
        let mut peer = connect_fast(&seed, b"-FK0001-").await?;
        peer.bitfield(&[0xff, 0xff, 0, 0, 0, 0, 0, 0]).await?;
        peer.request(30, 0, BLOCK).await?;
        peer.expect(Wire::RejectRequest(30, 0, BLOCK), STEP).await?;

        let flood: Vec<u8> = (0..MAX_REJECTS_PER_CHOKE + 50)
            .flat_map(|i| FakePeer::request_frame(31 + i % 8, 0, BLOCK))
            .collect();
        peer.send_raw(&flood).await?;
        // The unchoke is queued after every answer to the flood.
        peer.interested().await?;
        let mut rejects = 1;
        loop {
            match peer.recv(STEP).await {
                Recv::Msg(Wire::Unchoke) => break,
                Recv::Msg(Wire::RejectRequest(..)) => rejects += 1,
                Recv::Msg(Wire::Piece { .. }) => bail!("served a choked peer"),
                Recv::Msg(_) => {}
                Recv::Timeout | Recv::Closed => bail!("no unchoke after {rejects} rejects"),
            }
        }
        assert_eq!(rejects, MAX_REJECTS_PER_CHOKE);

        // Still connected, and served now.
        peer.request(30, 0, BLOCK).await?;
        let pieces = peer.expect_pieces(1, STEP).await?;
        assert_eq!((pieces[0].0, pieces[0].1), (30, 0));
        Ok(())
    })
    .await
}

// BEP 6: every request gets exactly one Piece or RejectRequest, a cancelled one included.
#[tokio::test(flavor = "multi_thread")]
async fn test_fast_cancel_of_a_queued_request_is_rejected() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts {
            upload_bps: Some(BLOCK),
            ..Default::default()
        })
        .await?;
        let mut peer = connect_fast(&seed, b"-FK0001-").await?;
        peer.have_none().await?;
        peer.interested().await?;
        peer.expect(Wire::Unchoke, STEP).await?;
        let asked = [
            (0, 0, BLOCK),
            (0, BLOCK, BLOCK),
            (1, 0, BLOCK),
            (1, BLOCK, BLOCK),
            (2, 0, BLOCK),
        ];
        for (piece, begin, len) in asked {
            peer.request(piece, begin, len).await?;
        }
        peer.cancel(1, BLOCK, BLOCK).await?;

        let mut served = Vec::new();
        let mut rejected = Vec::new();
        while served.len() < 4 {
            match peer.recv(STEP).await {
                Recv::Msg(Wire::Piece { index, begin, .. }) => served.push((index, begin)),
                Recv::Msg(Wire::RejectRequest(piece, begin, len)) => {
                    rejected.push((piece, begin, len))
                }
                Recv::Msg(_) => {}
                Recv::Timeout | Recv::Closed => {
                    bail!("served {served:?}, rejected {rejected:?}")
                }
            }
        }
        assert_eq!(rejected, vec![(1, BLOCK, BLOCK)]);
        assert_eq!(served, vec![(0, 0), (0, BLOCK), (1, 0), (2, 0)]);
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Allowed fast, uploading side
// ---------------------------------------------------------------------------------------------

// A choked newcomer is offered the canonical allowed-fast set, and served its blocks while
// choked, once each. Anything else is rejected.
#[tokio::test(flavor = "multi_thread")]
async fn test_fast_allowed_fast_set_is_offered_and_served_while_choked() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder_ext(
            SeederOpts::default(),
            SeederExt {
                pieces: Some(64),
                ..Default::default()
            },
        )
        .await?;
        let set = allowed_fast_set(Ipv4Addr::LOCALHOST, &seed.info_hash, 64, ALLOWED_FAST_K);
        assert_eq!(set.len(), ALLOWED_FAST_K as usize);

        let mut peer = connect_fast(&seed, b"-FK0001-").await?;
        assert_eq!(peer.first_message(STEP).await?, Wire::HaveAll);
        peer.have_none().await?;
        let mut offered = Vec::new();
        while offered.len() < set.len() {
            match peer.recv(STEP).await {
                Recv::Msg(Wire::AllowedFast(piece)) => offered.push(piece),
                Recv::Msg(Wire::KeepAlive | Wire::Extended) => {}
                Recv::Msg(m) => bail!("{m:?} among the allowed-fast pieces {offered:?}"),
                Recv::Timeout | Recv::Closed => bail!("offered {offered:?} only"),
            }
        }
        assert_eq!(offered, set);

        let fast_piece = set[0];
        let other = (0..64)
            .find(|p| !set.contains(p))
            .context("a piece not in the set")?;
        peer.request(fast_piece, BLOCK, BLOCK).await?;
        let pieces = peer.expect_pieces(1, STEP).await?;
        assert_eq!((pieces[0].0, pieces[0].1), (fast_piece, BLOCK));
        assert_eq!(pieces[0].2, seed.block(fast_piece, BLOCK, BLOCK));

        peer.request(other, 0, BLOCK).await?;
        peer.expect(Wire::RejectRequest(other, 0, BLOCK), STEP)
            .await?;
        // The same block again: it was served once while choked already.
        peer.request(fast_piece, BLOCK, BLOCK).await?;
        peer.expect(Wire::RejectRequest(fast_piece, BLOCK, BLOCK), STEP)
            .await?;
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Downloading side
// ---------------------------------------------------------------------------------------------

// A peer that chokes the client and rejects what it was asked for: the client gives the blocks
// back and completes the torrent from another peer, without asking the choking one again. The
// read timeout is long, so the safety net (half of it) cannot be what gives the blocks back.
#[tokio::test(flavor = "multi_thread")]
async fn test_fast_rejected_blocks_go_to_another_peer() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let client = start_client(
            &seed,
            ClientOpts {
                listen: true,
                read_write_timeout: Some(Duration::from_secs(120)),
                ..Default::default()
            },
        )
        .await?;

        let mut first = connect_fast(&client, b"-FK0001-").await?;
        first.have_all().await?;
        first.unchoke().await?;
        let asked = requests(&mut first, BLOCKS).await?;
        first.choke().await?;
        for &(piece, begin, len) in &asked {
            first.reject(piece, begin, len).await?;
        }

        let mut second = connect_fast(&client, b"-FK0002-").await?;
        second.have_all().await?;
        second.unchoke().await?;
        let again = serve_until_complete(&mut second, &seed, &client).await?;
        for block in &asked {
            assert!(again.contains(block), "{block:?} was not asked again");
        }
        let (msgs, _) = first.collect(Duration::ZERO).await;
        assert!(
            msgs.iter().all(|m| !matches!(m, Wire::Request(..))),
            "asked the choking peer again: {msgs:?}"
        );
        Ok(())
    })
    .await
}

/// Rejects every request `peer` receives during `window`, sending a redundant `Unchoke` after
/// each reject when `unchoke_each` is set. Returns the requests received, or an error when the
/// connection closed.
async fn reject_everything(
    peer: &mut FakePeer,
    window: Duration,
    unchoke_each: bool,
) -> anyhow::Result<usize> {
    let deadline = Instant::now() + window;
    let mut asked = 0;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(asked);
        }
        match peer.recv(left).await {
            Recv::Msg(Wire::Request(piece, begin, len)) => {
                asked += 1;
                peer.reject(piece, begin, len).await?;
                if unchoke_each {
                    peer.unchoke().await?;
                }
            }
            Recv::Msg(_) => {}
            Recv::Timeout => return Ok(asked),
            Recv::Closed => bail!("closed after {asked} requests"),
        }
    }
}

/// A client of a torrent with more pieces than the rejected list holds, and a peer that has
/// all of them and unchokes the client.
async fn many_pieces_and_an_unchoking_peer() -> anyhow::Result<(Fixture, Fixture, FakePeer)> {
    let seed = start_seeder_ext(
        SeederOpts::default(),
        SeederExt {
            pieces: Some(80),
            ..Default::default()
        },
    )
    .await?;
    let client = start_client(
        &seed,
        ClientOpts {
            listen: true,
            ..Default::default()
        },
    )
    .await?;
    let mut peer = connect_fast(&client, b"-FK0001-").await?;
    peer.have_all().await?;
    peer.unchoke().await?;
    Ok((seed, client, peer))
}

// Review scenario A: a peer that unchokes the client and rejects every request, with more
// pieces than the client remembers rejects of. The client stops asking after a streak of
// rejects instead of trying piece after piece at round-trip speed, and keeps the connection
// (the peer may serve later).
#[tokio::test(flavor = "multi_thread")]
async fn test_fast_rejecting_everything_is_not_a_request_loop() -> anyhow::Result<()> {
    run(async {
        let (_seed, client, mut peer) = many_pieces_and_an_unchoking_peer().await?;
        let asked = reject_everything(&mut peer, Duration::from_secs(3), false).await?;
        assert!(asked > 0, "nothing was asked");
        assert!(
            asked < MAX_REJECT_STREAK as usize,
            "asked {asked} times in a request/reject loop"
        );
        assert_eq!(live_fast_flags(&client), vec![true], "the peer was dropped");
        Ok(())
    })
    .await
}

// Review scenario B: the same peer sends a redundant `Unchoke` after every reject. Only a real
// choked-to-unchoked transition forgives rejects, so the loop is not reopened.
#[tokio::test(flavor = "multi_thread")]
async fn test_fast_redundant_unchokes_do_not_reopen_the_loop() -> anyhow::Result<()> {
    run(async {
        let (_seed, client, mut peer) = many_pieces_and_an_unchoking_peer().await?;
        let asked = reject_everything(&mut peer, Duration::from_secs(3), true).await?;
        assert!(asked > 0, "nothing was asked");
        assert!(
            asked < MAX_REJECT_STREAK as usize,
            "asked {asked} times in a request/reject loop"
        );
        assert_eq!(live_fast_flags(&client), vec![true], "the peer was dropped");
        Ok(())
    })
    .await
}

// A peer that unchokes the client and rejects everything is useless: a real choke and unchoke
// end the pause the streak started, but not the streak, and past `MAX_REJECT_STREAK` rejects in
// a row the client disconnects it.
#[tokio::test(flavor = "multi_thread")]
async fn test_fast_peer_rejecting_everything_is_disconnected() -> anyhow::Result<()> {
    run(async {
        let (_seed, _client, mut peer) = many_pieces_and_an_unchoking_peer().await?;
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut rejected = 0;
        loop {
            anyhow::ensure!(
                Instant::now() < deadline,
                "still connected after {rejected} rejects"
            );
            match peer.recv(Duration::from_millis(300)).await {
                Recv::Msg(Wire::Request(piece, begin, len)) => {
                    // The client closes the connection right after the reject that ends the
                    // streak, with more requests of it still to be answered here.
                    if peer.reject(piece, begin, len).await.is_err() {
                        break;
                    }
                    rejected += 1;
                }
                Recv::Msg(_) => {}
                // Paused: a choke and an unchoke make the client ask again.
                Recv::Timeout => {
                    peer.choke().await?;
                    peer.unchoke().await?;
                }
                Recv::Closed => break,
            }
        }
        assert!(
            rejected >= MAX_REJECT_STREAK,
            "disconnected after {rejected} rejects"
        );
        Ok(())
    })
    .await
}

// A peer that chokes the client and then neither delivers nor rejects (it only sends
// keep-alives, so the connection lives on) does not hold the pieces: they go back to the queue
// once the answer is overdue, half the read timeout after the choke, and not before.
#[tokio::test(flavor = "multi_thread")]
async fn test_fast_choke_without_answers_gives_pieces_back_in_time() -> anyhow::Result<()> {
    run(async {
        let read_write_timeout = Duration::from_secs(2);
        let seed = start_seeder(SeederOpts::default()).await?;
        let client = start_client(
            &seed,
            ClientOpts {
                listen: true,
                read_write_timeout: Some(read_write_timeout),
                ..Default::default()
            },
        )
        .await?;

        let mut silent = connect_fast(&client, b"-FK0001-").await?;
        silent.have_all().await?;
        silent.unchoke().await?;
        let asked = requests(&mut silent, BLOCKS).await?;
        let choked_at = Instant::now();
        silent.choke().await?;

        let mut other = connect_fast(&client, b"-FK0002-").await?;
        other.have_all().await?;
        other.unchoke().await?;
        let mut keep_alive = tokio::time::interval(Duration::from_millis(300));
        let first_request = loop {
            tokio::select! {
                _ = keep_alive.tick() => silent.keep_alive().await?,
                m = other.recv(STEP) => match m {
                    Recv::Msg(Wire::Request(piece, begin, len)) => {
                        break (choked_at.elapsed(), (piece, begin, len));
                    }
                    Recv::Msg(_) => {}
                    Recv::Timeout | Recv::Closed => bail!("nothing given back"),
                },
            }
        };
        let (elapsed, (piece, begin, len)) = first_request;
        assert!(
            elapsed >= read_write_timeout / 2,
            "given back {elapsed:?} after the choke, before the answers were overdue"
        );
        assert!(
            elapsed < read_write_timeout * 2,
            "given back {elapsed:?} after the choke"
        );
        other
            .piece(piece, begin, seed.block(piece, begin, len))
            .await?;
        let mut again = serve_until_complete(&mut other, &seed, &client).await?;
        again.insert((piece, begin, len));
        for block in &asked {
            assert!(again.contains(block), "{block:?} was not asked again");
        }
        Ok(())
    })
    .await
}

// A silent peer, not even keep-alives: the safety net gives the pieces back before the read
// timeout gives up on the peer.
#[tokio::test(flavor = "multi_thread")]
async fn test_fast_choke_and_silence_gives_pieces_back() -> anyhow::Result<()> {
    run(async {
        let read_write_timeout = Duration::from_secs(2);
        let seed = start_seeder(SeederOpts::default()).await?;
        let client = start_client(
            &seed,
            ClientOpts {
                listen: true,
                read_write_timeout: Some(read_write_timeout),
                ..Default::default()
            },
        )
        .await?;

        let mut silent = connect_fast(&client, b"-FK0001-").await?;
        silent.have_all().await?;
        silent.unchoke().await?;
        let asked = requests(&mut silent, BLOCKS).await?;
        let choked_at = Instant::now();
        silent.choke().await?;

        let mut other = connect_fast(&client, b"-FK0002-").await?;
        other.have_all().await?;
        other.unchoke().await?;
        let again = serve_until_complete(&mut other, &seed, &client).await?;
        for block in &asked {
            assert!(again.contains(block), "{block:?} was not asked again");
        }
        assert!(
            choked_at.elapsed() < read_write_timeout * 3,
            "completed {:?} after the choke",
            choked_at.elapsed()
        );
        Ok(())
    })
    .await
}

// A peer that chokes the client but allows some pieces fast: the client asks for exactly those
// while choked, and for the rest once unchoked.
#[tokio::test(flavor = "multi_thread")]
async fn test_fast_client_requests_allowed_fast_pieces_while_choked() -> anyhow::Result<()> {
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
        let mut peer = connect_fast(&client, b"-FK0001-").await?;
        peer.have_all().await?;
        peer.allowed_fast(2).await?;
        // Out of range: ignored.
        peer.allowed_fast(1000).await?;

        let mut while_choked = Vec::new();
        let done = async {
            wait_until(
                || {
                    anyhow::ensure!(progress(&client) == u64::from(PIECE_LEN));
                    Ok(())
                },
                STEP,
            )
            .await
        };
        tokio::pin!(done);
        loop {
            tokio::select! {
                r = &mut done => {
                    r?;
                    break;
                }
                m = peer.recv(STEP) => match m {
                    Recv::Msg(Wire::Request(piece, begin, len)) => {
                        while_choked.push((piece, begin, len));
                        peer.piece(piece, begin, seed.block(piece, begin, len)).await?;
                    }
                    Recv::Msg(_) => {}
                    Recv::Timeout | Recv::Closed => bail!("asked for {while_choked:?} only"),
                },
            }
        }
        while_choked.sort_unstable();
        assert_eq!(while_choked, vec![(2, 0, BLOCK), (2, BLOCK, BLOCK)]);

        peer.unchoke().await?;
        let rest = serve_until_complete(&mut peer, &seed, &client).await?;
        assert_eq!(rest.len(), BLOCKS - 2);
        assert!(rest.iter().all(|&(piece, ..)| piece != 2), "{rest:?}");
        Ok(())
    })
    .await
}

// A suggested piece is the next one asked for.
#[tokio::test(flavor = "multi_thread")]
async fn test_fast_suggest_piece_steers_the_next_request() -> anyhow::Result<()> {
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
        let mut peer = connect_fast(&client, b"-FK0001-").await?;
        peer.have_all().await?;
        // Out of range: ignored.
        peer.suggest_piece(1000).await?;
        peer.suggest_piece(3).await?;
        peer.unchoke().await?;
        let first = requests(&mut peer, 1).await?;
        assert_eq!(first[0].0, 3, "asked for {first:?}");
        Ok(())
    })
    .await
}

// HaveAll is a full bitfield: the peer counts as a seeder, and the download works.
#[tokio::test(flavor = "multi_thread")]
async fn test_fast_have_all_peer_is_a_seeder() -> anyhow::Result<()> {
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
        let mut peer = connect_fast(&client, b"-FK0001-").await?;
        peer.have_all().await?;
        peer.expect(Wire::Interested, STEP).await?;
        let live = client.handle.live().context("client not live")?;
        assert_eq!(live.stats_snapshot().peer_stats.live_seeders, 1);
        assert_eq!(live_fast_flags(&client), vec![true]);

        peer.unchoke().await?;
        let asked = serve_until_complete(&mut peer, &seed, &client).await?;
        assert_eq!(asked.len(), BLOCKS);
        Ok(())
    })
    .await
}

// HaveNone is an empty bitfield: the client is not interested, and the peer does not count as
// a seeder.
#[tokio::test(flavor = "multi_thread")]
async fn test_fast_have_none_peer_has_nothing() -> anyhow::Result<()> {
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
        let mut peer = connect_fast(&client, b"-FK0001-").await?;
        peer.have_none().await?;
        // The client becomes interested only with the Have, which proves HaveNone was handled
        // before it.
        peer.have(1).await?;
        peer.expect(Wire::Interested, STEP).await?;
        let live = client.handle.live().context("client not live")?;
        assert_eq!(live.stats_snapshot().peer_stats.live_seeders, 0);
        Ok(())
    })
    .await
}

// A Fast message on a connection without the Fast Extension, or a second piece set on one with
// it, is a protocol error: the connection is closed.
#[tokio::test(flavor = "multi_thread")]
async fn test_fast_protocol_errors_close_the_connection() -> anyhow::Result<()> {
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

        let mut plain = connect_plain(&client, b"-FK0001-").await?;
        plain.have_all().await?;
        plain.expect_closed(STEP).await?;

        let mut twice = connect_fast(&client, b"-FK0002-").await?;
        twice.have_all().await?;
        twice.bitfield(&[0b1111_0000]).await?;
        twice.expect_closed(STEP).await?;

        let mut late = connect_fast(&client, b"-FK0003-").await?;
        late.have(1).await?;
        late.have_none().await?;
        late.expect_closed(STEP).await?;
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Two sessions
// ---------------------------------------------------------------------------------------------

// Two sessions complete a download with the Fast Extension on both sides, and with it disabled
// on either side, where it is not negotiated.
#[tokio::test(flavor = "multi_thread")]
async fn test_fast_two_sessions_download() -> anyhow::Result<()> {
    run(async {
        for (seed_disables, client_disables) in [(false, false), (true, false), (false, true)] {
            // Slow enough to see the connection live before the download completes.
            let seed = start_seeder_ext(
                SeederOpts {
                    upload_bps: Some(4 * BLOCK),
                    ..Default::default()
                },
                SeederExt {
                    disable_fast_extension: seed_disables,
                    ..Default::default()
                },
            )
            .await?;
            let client = start_client(
                &seed,
                ClientOpts {
                    initial_peers: vec![seed.addr()],
                    disable_fast_extension: client_disables,
                    ..Default::default()
                },
            )
            .await?;
            let expected = !seed_disables && !client_disables;
            wait_until(
                || {
                    let flags = live_fast_flags(&client);
                    anyhow::ensure!(flags == vec![expected], "live peers: {flags:?}");
                    Ok(())
                },
                STEP,
            )
            .await
            .with_context(|| {
                format!("seed_disables={seed_disables} client_disables={client_disables}")
            })?;
            tokio::time::timeout(
                Duration::from_secs(20),
                client.handle.wait_until_completed(),
            )
            .await
            .context("download did not complete")??;
        }
        Ok(())
    })
    .await
}
