//! Tests of the peer policy hooks.
//!
//! They run a real session on loopback and talk to it with the wire-level fake peer of
//! `fake_peer.rs`, so every message, and the moment it is sent, is under the test's control.

use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::NonZeroU32,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use bitvec::{order::Msb0, slice::BitSlice};
use librqbit_core::Id20;
use parking_lot::Mutex;
use tempfile::TempDir;
use tokio::{net::TcpListener, time::timeout};

use super::fake_peer::{
    BLOCK, ClientOpts, FILE_LEN, FakePeer, Fixture, PIECE_LEN, PIECES, SeederOpts, Wire,
    handshake_bytes, peer_id, run, start_client, start_seeder, uploaded,
};
use crate::{
    AddTorrent, AddTorrentOptions, ConnectionOptions, PeerCtx, PeerEvent, PeerPolicy, PeerVerdict,
    Session, SessionOptions, tests::test_util::wait_until,
};

fn blocked_incoming(s: &Session) -> u64 {
    s.stats.counters.blocked_incoming.load(Ordering::Relaxed)
}

fn blocked_outgoing(s: &Session) -> u64 {
    s.stats.counters.blocked_outgoing.load(Ordering::Relaxed)
}

async fn wait_for(
    what: &str,
    mut cond: impl FnMut() -> bool,
    within: Duration,
) -> anyhow::Result<()> {
    wait_until(
        || {
            if cond() { Ok(()) } else { bail!("{what}") }
        },
        within,
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// The policy under test
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Ev {
    Handshake {
        peer_id: Id20,
        reserved: u64,
        incoming: bool,
    },
    ExtendedHandshake(Option<String>),
    Bitfield(Vec<bool>),
    Have(u32),
    Request {
        piece: u32,
        begin: u32,
        length: u32,
        claims: bool,
    },
    Cancel(u32, u32, u32),
    Uploaded(u32, u32, u32),
    Disconnected(Option<Vec<bool>>),
}

fn bits(b: &BitSlice<u8, Msb0>) -> Vec<bool> {
    b.iter().by_vals().collect()
}

impl From<&PeerEvent<'_>> for Ev {
    fn from(e: &PeerEvent<'_>) -> Self {
        match *e {
            PeerEvent::Handshake {
                peer_id,
                reserved,
                incoming,
            } => Ev::Handshake {
                peer_id,
                reserved,
                incoming,
            },
            PeerEvent::ExtendedHandshake { client } => {
                Ev::ExtendedHandshake(client.map(str::to_owned))
            }
            PeerEvent::Bitfield { bits: b } => Ev::Bitfield(bits(b)),
            PeerEvent::Have { piece } => Ev::Have(piece),
            PeerEvent::Request {
                piece,
                begin,
                length,
                peer_claims_piece,
            } => Ev::Request {
                piece,
                begin,
                length,
                claims: peer_claims_piece,
            },
            PeerEvent::Cancel {
                piece,
                begin,
                length,
            } => Ev::Cancel(piece, begin, length),
            PeerEvent::Uploaded {
                piece,
                begin,
                length,
            } => Ev::Uploaded(piece, begin, length),
            PeerEvent::Disconnected { bits: b } => Ev::Disconnected(b.map(bits)),
        }
    }
}

#[derive(Debug, Clone)]
struct CtxCopy {
    info_hash: Id20,
    addr: SocketAddr,
    private: bool,
    total_length: u64,
    piece_length: u32,
    num_pieces: u32,
    we_are_seeding: bool,
}

struct Seen {
    ctx: CtxCopy,
    ev: Ev,
}

type Rule = Box<dyn Fn(&Ev) -> PeerVerdict + Send + Sync>;

struct TestPolicy {
    admit: AtomicBool,
    admit_calls: Mutex<Vec<IpAddr>>,
    readmit_calls: Mutex<Vec<IpAddr>>,
    seen: Mutex<Vec<Seen>>,
    rule: Mutex<Option<Rule>>,
}

impl TestPolicy {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            admit: AtomicBool::new(true),
            admit_calls: Default::default(),
            readmit_calls: Default::default(),
            seen: Default::default(),
            rule: Default::default(),
        })
    }

    fn with_rule(rule: impl Fn(&Ev) -> PeerVerdict + Send + Sync + 'static) -> Arc<Self> {
        let p = Self::new();
        *p.rule.lock() = Some(Box::new(rule));
        p
    }

    fn set_admit(&self, admit: bool) {
        self.admit.store(admit, Ordering::SeqCst);
    }

    fn admit_calls(&self) -> Vec<IpAddr> {
        self.admit_calls.lock().clone()
    }

    fn readmit_calls(&self) -> Vec<IpAddr> {
        self.readmit_calls.lock().clone()
    }

    fn events(&self) -> Vec<Ev> {
        self.seen.lock().iter().map(|s| s.ev.clone()).collect()
    }

    fn events_of(&self, addr: SocketAddr) -> Vec<Ev> {
        self.seen
            .lock()
            .iter()
            .filter(|s| s.ctx.addr == addr)
            .map(|s| s.ev.clone())
            .collect()
    }

    fn count(&self, f: impl Fn(&Ev) -> bool) -> usize {
        self.seen.lock().iter().filter(|s| f(&s.ev)).count()
    }

    fn handshakes(&self) -> usize {
        self.count(|e| matches!(e, Ev::Handshake { .. }))
    }
}

impl PeerPolicy for TestPolicy {
    fn admit(&self, ip: IpAddr) -> bool {
        self.admit_calls.lock().push(ip);
        self.admit.load(Ordering::SeqCst)
    }

    fn readmit(&self, ip: IpAddr) -> bool {
        self.readmit_calls.lock().push(ip);
        self.admit.load(Ordering::SeqCst)
    }

    fn on_event(&self, ctx: &PeerCtx, event: PeerEvent<'_>) -> PeerVerdict {
        let ev = Ev::from(&event);
        let verdict = self
            .rule
            .lock()
            .as_ref()
            .map_or(PeerVerdict::Allow, |rule| rule(&ev));
        self.seen.lock().push(Seen {
            ctx: CtxCopy {
                info_hash: ctx.info_hash,
                addr: ctx.addr,
                private: ctx.private,
                total_length: ctx.total_length,
                piece_length: ctx.piece_length,
                num_pieces: ctx.num_pieces,
                we_are_seeding: ctx.we_are_seeding,
            },
            ev,
        });
        verdict
    }
}

const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

// ---------------------------------------------------------------------------------------------
// Generic fixes
// ---------------------------------------------------------------------------------------------

// When the torrent completes, the peers that have everything are dropped. The Disconnected event
// is reported once either way.
#[tokio::test(flavor = "multi_thread")]
async fn test_disconnected_is_reported_when_the_download_completes() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let policy = TestPolicy::new();
        let client = start_client(
            &seed,
            ClientOpts {
                initial_peers: vec![seed.addr()],
                policy: Some(policy.clone()),
                ..Default::default()
            },
        )
        .await?;
        timeout(
            Duration::from_secs(20),
            client.handle.wait_until_completed(),
        )
        .await??;
        wait_for(
            "disconnected event",
            || policy.count(|e| matches!(e, Ev::Disconnected(_))) == 1,
            Duration::from_secs(5),
        )
        .await?;
        let seen = policy.seen.lock();
        let last = seen.last().unwrap();
        // Either the torrent already dropped the state of the peer (no bitfield), or the
        // requester got to disconnect it first (the complete bitfield it announced).
        match &last.ev {
            Ev::Disconnected(None) => {}
            Ev::Disconnected(Some(b)) => assert_eq!(b, &vec![true; PIECES as usize]),
            other => bail!("unexpected last event {other:?}"),
        }
        assert!(last.ctx.we_are_seeding);
        assert!(matches!(
            seen[0].ev,
            Ev::Handshake {
                incoming: false,
                ..
            }
        ));
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_event_sequence_of_a_connection() -> anyhow::Result<()> {
    run(async {
        let policy = TestPolicy::new();
        let seed = start_seeder(SeederOpts {
            policy: Some(policy.clone()),
            ..Default::default()
        })
        .await?;
        let id = peer_id(b"-FK0002-");
        let mut peer = FakePeer::connect(seed.addr(), seed.info_hash, &id).await?;
        let me = peer.local;
        peer.extended_handshake(b"d1:v9:FakeLeeche").await?;
        // Pieces 0 and 2 through the bitfield, 3 through HAVE.
        peer.bitfield(&[0b1010_0000]).await?;
        peer.have(3).await?;
        peer.interested().await?;
        // 0 and 3 were announced, 1 never was.
        peer.request(0, 0, BLOCK).await?;
        peer.request(1, 0, BLOCK).await?;
        peer.request(3, BLOCK, BLOCK).await?;
        // Nothing is queued for this one: the cancel is reported, and has no other effect.
        peer.cancel(2, 0, BLOCK).await?;
        let pieces = peer.expect_pieces(3, Duration::from_secs(10)).await?;
        for (piece, begin, data) in &pieces {
            assert_eq!(data, seed.block(*piece, *begin, BLOCK));
        }
        wait_for(
            "uploaded events",
            || policy.count(|e| matches!(e, Ev::Uploaded(..))) == 3,
            Duration::from_secs(5),
        )
        .await?;
        drop(peer);
        wait_for(
            "disconnected event",
            || policy.count(|e| matches!(e, Ev::Disconnected(_))) == 1,
            Duration::from_secs(5),
        )
        .await?;

        let mut events = policy.events_of(me);
        // Uploads are written concurrently with the reads, compare them separately.
        let mut uploads: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Ev::Uploaded(p, b, l) => Some((*p, *b, *l)),
                _ => None,
            })
            .collect();
        uploads.sort_unstable();
        assert_eq!(
            uploads,
            vec![(0, 0, BLOCK), (1, 0, BLOCK), (3, BLOCK, BLOCK)]
        );
        events.retain(|e| !matches!(e, Ev::Uploaded(..)));

        let Some(Ev::Handshake {
            peer_id,
            reserved,
            incoming,
        }) = events.first().cloned()
        else {
            bail!("first event is not a handshake: {events:?}");
        };
        assert_eq!(peer_id.0, id);
        assert!(incoming);
        assert_ne!(reserved & (1 << 20), 0, "extension bit of the peer");
        assert_eq!(
            events[1..],
            [
                Ev::ExtendedHandshake(Some("FakeLeech".into())),
                Ev::Bitfield(vec![true, false, true, false]),
                Ev::Have(3),
                Ev::Request {
                    piece: 0,
                    begin: 0,
                    length: BLOCK,
                    claims: true
                },
                Ev::Request {
                    piece: 1,
                    begin: 0,
                    length: BLOCK,
                    claims: false
                },
                Ev::Request {
                    piece: 3,
                    begin: BLOCK,
                    length: BLOCK,
                    claims: true
                },
                Ev::Cancel(2, 0, BLOCK),
                Ev::Disconnected(Some(vec![true, false, true, true])),
            ]
        );

        // Every event carries the same description of the connection and the torrent.
        for s in policy.seen.lock().iter().filter(|s| s.ctx.addr == me) {
            assert_eq!(s.ctx.info_hash, seed.info_hash);
            assert!(!s.ctx.private);
            assert_eq!(s.ctx.total_length, FILE_LEN as u64);
            assert_eq!(s.ctx.piece_length, PIECE_LEN);
            assert_eq!(s.ctx.num_pieces, PIECES);
            assert!(s.ctx.we_are_seeding);
        }
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Verdicts
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_choke_stops_uploads_and_keeps_the_connection() -> anyhow::Result<()> {
    run(async {
        let policy = TestPolicy::with_rule(|e| match e {
            // The peer asking for piece 1 is the one to choke.
            Ev::Request { piece: 1, .. } => PeerVerdict::Choke,
            _ => PeerVerdict::Allow,
        });
        let seed = start_seeder(SeederOpts {
            policy: Some(policy.clone()),
            // So that accepted requests stay queued for a while.
            upload_bps: Some(BLOCK),
            ..Default::default()
        })
        .await?;
        let mut peer =
            FakePeer::connect(seed.addr(), seed.info_hash, &peer_id(b"-FK0003-")).await?;
        peer.interested().await?;
        let mut batch = Vec::new();
        // The first goes out immediately, the next two wait for their turn, the last one chokes.
        for (piece, begin) in [(0, 0), (0, BLOCK), (2, 0), (1, 0)] {
            batch.extend(FakePeer::request_frame(piece, begin, BLOCK));
        }
        peer.send_raw(&batch).await?;

        let (msgs, closed) = peer.collect(Duration::from_millis(3500)).await;
        assert!(!closed, "choked peers stay connected");
        let pieces: Vec<_> = msgs
            .iter()
            .filter_map(|m| match m {
                Wire::Piece { index, begin, .. } => Some((*index, *begin)),
                _ => None,
            })
            .collect();
        assert_eq!(
            pieces,
            vec![(0, 0)],
            "queued requests are dropped: {msgs:?}"
        );
        assert_eq!(
            msgs.iter().filter(|m| **m == Wire::Choke).count(),
            1,
            "exactly one choke: {msgs:?}"
        );

        // Requests after the choke are ignored, silently, but still reported.
        peer.request(0, 0, BLOCK).await?;
        peer.request(1, 0, BLOCK).await?;
        let (msgs, closed) = peer.collect(Duration::from_millis(1500)).await;
        assert!(!closed);
        assert!(
            msgs.iter()
                .all(|m| !matches!(m, Wire::Piece { .. } | Wire::Choke)),
            "{msgs:?}"
        );
        wait_for(
            "requests reported",
            || policy.count(|e| matches!(e, Ev::Request { .. })) == 6,
            Duration::from_secs(5),
        )
        .await?;
        assert_eq!(uploaded(&seed), BLOCK as u64);
        assert_eq!(policy.count(|e| matches!(e, Ev::Uploaded(..))), 1);

        // The connection is still alive in the session as well.
        assert_eq!(seed.session.disconnect_peers_where(|_| true), 1);
        peer.expect_closed(Duration::from_secs(5)).await?;
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_disconnect_verdicts_close_the_connection_at_every_hook() -> anyhow::Result<()> {
    run(async {
        // On the handshake.
        let policy = TestPolicy::with_rule(|e| match e {
            Ev::Handshake { .. } => PeerVerdict::Disconnect,
            _ => PeerVerdict::Allow,
        });
        let seed = start_seeder(SeederOpts {
            policy: Some(policy.clone()),
            ..Default::default()
        })
        .await?;
        let mut peer =
            FakePeer::connect(seed.addr(), seed.info_hash, &peer_id(b"-FK0004-")).await?;
        peer.expect_closed(Duration::from_secs(5)).await?;
        wait_for(
            "disconnected",
            || policy.count(|e| matches!(e, Ev::Disconnected(_))) == 1,
            Duration::from_secs(5),
        )
        .await?;
        assert_eq!(policy.events().len(), 2, "{:?}", policy.events());

        // On the bitfield.
        let policy = TestPolicy::with_rule(|e| match e {
            Ev::Bitfield(_) => PeerVerdict::Disconnect,
            _ => PeerVerdict::Allow,
        });
        let seed = start_seeder(SeederOpts {
            policy: Some(policy.clone()),
            ..Default::default()
        })
        .await?;
        let mut peer =
            FakePeer::connect(seed.addr(), seed.info_hash, &peer_id(b"-FK0004-")).await?;
        peer.bitfield(&[0xf0]).await?;
        peer.expect_closed(Duration::from_secs(5)).await?;
        // The bitfield was recorded before the disconnect: it is the one reported with it.
        wait_for(
            "disconnected",
            || policy.count(|e| matches!(e, Ev::Disconnected(_))) == 1,
            Duration::from_secs(5),
        )
        .await?;
        assert_eq!(
            policy.events().last(),
            Some(&Ev::Disconnected(Some(vec![true; 4])))
        );

        // On a request: nothing is sent.
        let policy = TestPolicy::with_rule(|e| match e {
            Ev::Request { .. } => PeerVerdict::Disconnect,
            _ => PeerVerdict::Allow,
        });
        let seed = start_seeder(SeederOpts {
            policy: Some(policy.clone()),
            ..Default::default()
        })
        .await?;
        let mut peer =
            FakePeer::connect(seed.addr(), seed.info_hash, &peer_id(b"-FK0004-")).await?;
        peer.request(0, 0, BLOCK).await?;
        let msgs = peer.expect_closed(Duration::from_secs(5)).await?;
        assert!(
            msgs.iter().all(|m| !matches!(m, Wire::Piece { .. })),
            "{msgs:?}"
        );
        assert_eq!(uploaded(&seed), 0);

        // On the upload itself: the block went out, the connection ends after it.
        let policy = TestPolicy::with_rule(|e| match e {
            Ev::Uploaded(..) => PeerVerdict::Disconnect,
            _ => PeerVerdict::Allow,
        });
        let seed = start_seeder(SeederOpts {
            policy: Some(policy.clone()),
            ..Default::default()
        })
        .await?;
        let mut peer =
            FakePeer::connect(seed.addr(), seed.info_hash, &peer_id(b"-FK0004-")).await?;
        peer.request(0, 0, BLOCK).await?;
        peer.request(0, BLOCK, BLOCK).await?;
        let (msgs, closed) = peer.collect(Duration::from_secs(5)).await;
        assert!(closed);
        let pieces = msgs
            .iter()
            .filter(|m| matches!(m, Wire::Piece { .. }))
            .count();
        assert_eq!(
            pieces, 1,
            "the connection ends after the first block: {msgs:?}"
        );
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_throttled_peer_does_not_hold_back_normal_peers() -> anyhow::Result<()> {
    run(async {
        let policy = TestPolicy::with_rule(|e| match e {
            Ev::Handshake { peer_id, .. } if peer_id.0.starts_with(b"-TH") => PeerVerdict::Throttle,
            _ => PeerVerdict::Allow,
        });
        let seed = start_seeder(SeederOpts {
            policy: Some(policy.clone()),
            ..Default::default()
        })
        .await?;
        // A block takes 15 seconds at this rate.
        seed.session.set_throttled_upload_bps(NonZeroU32::new(1024));
        assert_eq!(seed.session.throttled_upload_bps(), NonZeroU32::new(1024));

        let mut slow =
            FakePeer::connect(seed.addr(), seed.info_hash, &peer_id(b"-TH0001-")).await?;
        let mut fast =
            FakePeer::connect(seed.addr(), seed.info_hash, &peer_id(b"-NO0001-")).await?;
        slow.request(0, 0, BLOCK).await?;
        slow.request(0, BLOCK, BLOCK).await?;
        // Make sure the throttled requests are in front of the normal ones.
        wait_for(
            "throttled requests",
            || policy.count(|e| matches!(e, Ev::Request { .. })) == 2,
            Duration::from_secs(5),
        )
        .await?;
        for (piece, begin) in [(1, 0), (1, BLOCK), (2, 0), (2, BLOCK)] {
            fast.request(piece, begin, BLOCK).await?;
        }

        // Served long before the 1 KiB/s of the throttled peer would let a single block through:
        // the 5 s limit of `expect_pieces` is the only clock this relies on.
        let got = fast.expect_pieces(4, Duration::from_secs(5)).await?;
        for (piece, begin, data) in &got {
            assert_eq!(data, seed.block(*piece, *begin, BLOCK));
        }
        let (msgs, closed) = slow.collect(Duration::from_millis(1500)).await;
        assert!(!closed);
        assert!(
            msgs.iter().all(|m| !matches!(m, Wire::Piece { .. })),
            "a throttled block came faster than the rate allows: {} msgs",
            msgs.len()
        );
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_throttled_peers_share_the_default_rate() -> anyhow::Result<()> {
    run(async {
        let policy = TestPolicy::with_rule(|e| match e {
            Ev::Handshake { .. } => PeerVerdict::Throttle,
            _ => PeerVerdict::Allow,
        });
        let seed = start_seeder(SeederOpts {
            policy: Some(policy.clone()),
            ..Default::default()
        })
        .await?;
        assert_eq!(
            seed.session.throttled_upload_bps(),
            NonZeroU32::new(32 * 1024)
        );

        // 32 KiB/s: two blocks out of the initial burst, then one every half a second.
        let mut peer =
            FakePeer::connect(seed.addr(), seed.info_hash, &peer_id(b"-FK0005-")).await?;
        // At most as many requests as a throttled connection may have waiting (more would be
        // dropped, by design), so every round waits for its blocks.
        let started = Instant::now();
        for piece in 0..3 {
            peer.request(piece, 0, BLOCK).await?;
            peer.request(piece, BLOCK, BLOCK).await?;
            peer.expect_pieces(2, Duration::from_secs(30)).await?;
        }
        // Only a lower bound: a slow machine can only make this longer.
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(1400), "too fast: {took:?}");

        // Lifting the limit applies to what is requested from now on. At 32 KiB/s this much (48 blocks) would
        // take over 20 seconds, so getting it all within the timeout shows that the limit is gone,
        // by a margin that doesn't depend on how fast the machine is.
        seed.session.set_throttled_upload_bps(None);
        assert_eq!(seed.session.throttled_upload_bps(), None);
        timeout(Duration::from_secs(15), async {
            for _ in 0..12 {
                for piece in 0..2 {
                    peer.request(piece, 0, BLOCK).await?;
                    peer.request(piece, BLOCK, BLOCK).await?;
                }
                peer.expect_pieces(4, Duration::from_secs(15)).await?;
            }
            anyhow::Ok(())
        })
        .await
        .context("the lifted limit still applies")??;
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_throttled_request_cancelled_in_the_throttle_queue_is_not_sent() -> anyhow::Result<()>
{
    run(async {
        let policy = TestPolicy::with_rule(|e| match e {
            Ev::Handshake { .. } => PeerVerdict::Throttle,
            _ => PeerVerdict::Allow,
        });
        let seed = start_seeder(SeederOpts {
            policy: Some(policy.clone()),
            ..Default::default()
        })
        .await?;
        seed.session
            .set_throttled_upload_bps(NonZeroU32::new(BLOCK));
        let mut peer =
            FakePeer::connect(seed.addr(), seed.info_hash, &peer_id(b"-FK0005-")).await?;
        let mut batch = Vec::new();
        for (piece, begin) in [(0, 0), (0, BLOCK), (1, 0)] {
            batch.extend(FakePeer::request_frame(piece, begin, BLOCK));
        }
        batch.extend(FakePeer::frame(8, &FakePeer::triple(0, BLOCK, BLOCK)));
        peer.send_raw(&batch).await?;
        let got = peer.expect_pieces(2, Duration::from_secs(10)).await?;
        let got: Vec<_> = got.iter().map(|(p, b, _)| (*p, *b)).collect();
        assert_eq!(got, vec![(0, 0), (1, 0)]);
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Admission
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_refused_incoming_connection_is_dropped_before_the_handshake() -> anyhow::Result<()> {
    run(async {
        let policy = TestPolicy::new();
        policy.set_admit(false);
        let seed = start_seeder(SeederOpts {
            policy: Some(policy.clone()),
            ..Default::default()
        })
        .await?;

        let mut peer = FakePeer::connect_without_handshake(seed.addr()).await?;
        peer.send_raw(&handshake_bytes(seed.info_hash, &peer_id(b"-FK0006-")))
            .await?;
        let msgs = peer.expect_closed(Duration::from_secs(5)).await?;
        assert!(msgs.is_empty());
        assert!(peer.buf.is_empty(), "no handshake reply");
        assert_eq!(policy.admit_calls(), vec![LOOPBACK]);
        assert!(policy.events().is_empty());
        assert_eq!(blocked_incoming(&seed.session), 1);

        // Once admitted the same address connects.
        policy.set_admit(true);
        let _peer = FakePeer::connect(seed.addr(), seed.info_hash, &peer_id(b"-FK0006-")).await?;
        wait_for(
            "handshake",
            || policy.handshakes() == 1,
            Duration::from_secs(5),
        )
        .await?;
        assert_eq!(blocked_incoming(&seed.session), 1);
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_refused_outgoing_peer_is_not_dialed() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let policy = TestPolicy::new();
        policy.set_admit(false);
        // IPv4-mapped on purpose: the policy must see plain IPv4.
        let mapped = SocketAddr::new(
            IpAddr::V6(Ipv4Addr::LOCALHOST.to_ipv6_mapped()),
            listener.local_addr()?.port(),
        );
        let client = start_client(
            &seed,
            ClientOpts {
                policy: Some(policy.clone()),
                initial_peers: vec![mapped],
                dual_stack: true,
                ..Default::default()
            },
        )
        .await?;
        wait_for(
            "refusal",
            || blocked_outgoing(&client.session) == 1,
            Duration::from_secs(5),
        )
        .await?;
        assert!(
            timeout(Duration::from_millis(1500), listener.accept())
                .await
                .is_err(),
            "a refused address was dialed"
        );
        assert_eq!(policy.admit_calls(), vec![LOOPBACK]);
        assert!(policy.events().is_empty());
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_admitted_outgoing_peer_is_dialed() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let policy = TestPolicy::new();
        let _client = start_client(
            &seed,
            ClientOpts {
                policy: Some(policy.clone()),
                initial_peers: vec![listener.local_addr()?],
                ..Default::default()
            },
        )
        .await?;
        let (_conn, _) = timeout(Duration::from_secs(10), listener.accept())
            .await
            .context("an admitted address was not dialed")??;
        assert_eq!(policy.admit_calls(), vec![LOOPBACK]);
        Ok(())
    })
    .await
}

// Peers that were dropped because both sides had everything are brought back when more files are
// selected, unless the policy has refused them in the meantime.
#[tokio::test(flavor = "multi_thread")]
async fn test_refused_peers_are_not_reconnected_when_more_files_are_selected() -> anyhow::Result<()>
{
    run(async {
        let seed_policy = TestPolicy::new();
        let seed = start_seeder(SeederOpts {
            policy: Some(seed_policy.clone()),
            files: Some(2),
            ..Default::default()
        })
        .await?;
        let policy = TestPolicy::new();
        let client = start_client(
            &seed,
            ClientOpts {
                policy: Some(policy.clone()),
                initial_peers: vec![seed.addr()],
                only_files: Some(vec![0]),
                ..Default::default()
            },
        )
        .await?;
        timeout(
            Duration::from_secs(20),
            client.handle.wait_until_completed(),
        )
        .await??;
        wait_for(
            "the seeder lost the client",
            || seed_policy.count(|e| matches!(e, Ev::Disconnected(_))) == 1,
            Duration::from_secs(10),
        )
        .await?;
        assert_eq!(seed_policy.handshakes(), 1);

        // Refused: selecting the second file queues the seeder again, and its dial is refused.
        // The client marks the peer as not needed right after it disconnects it.
        tokio::time::sleep(Duration::from_millis(500)).await;
        policy.set_admit(false);
        let calls = policy.admit_calls().len();
        let both = HashSet::from([0, 1]);
        client
            .session
            .update_only_files(&client.handle, &both)
            .await?;
        wait_for(
            "the dial was refused",
            || blocked_outgoing(&client.session) == 1,
            Duration::from_secs(5),
        )
        .await?;
        assert!(
            policy.admit_calls().len() > calls,
            "the policy was not asked"
        );
        tokio::time::sleep(Duration::from_millis(1000)).await;
        assert_eq!(seed_policy.handshakes(), 1, "refused peer was reconnected");
        assert!(!client.handle.live().context("live")?.is_finished());

        // Asking again while it is refused dials nothing, and isn't a connection attempt.
        let (admits, readmits) = (policy.admit_calls().len(), policy.readmit_calls().len());
        assert_eq!(client.session.readmit_refused_peers(), 0);
        assert!(policy.readmit_calls().len() > readmits);
        assert_eq!(policy.admit_calls().len(), admits);

        // Admitted again: it is dialed, the peer was not forgotten.
        policy.set_admit(true);
        assert_eq!(client.session.readmit_refused_peers(), 1);
        timeout(
            Duration::from_secs(20),
            client.handle.wait_until_completed(),
        )
        .await??;
        assert_eq!(seed_policy.handshakes(), 2);
        assert_eq!(blocked_outgoing(&client.session), 1);
        Ok(())
    })
    .await
}

// The seeding state reported to the policy follows the selection: it is false while a selected
// file is missing and true once the files selected later are complete.
#[tokio::test(flavor = "multi_thread")]
async fn test_seeding_state_follows_a_changed_selection() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts {
            files: Some(2),
            ..Default::default()
        })
        .await?;
        let policy = TestPolicy::new();
        let client = start_client(
            &seed,
            ClientOpts {
                policy: Some(policy.clone()),
                initial_peers: vec![seed.addr()],
                only_files: Some(vec![0]),
                ..Default::default()
            },
        )
        .await?;
        timeout(
            Duration::from_secs(20),
            client.handle.wait_until_completed(),
        )
        .await??;
        wait_for(
            "the first connection ended",
            || policy.count(|e| matches!(e, Ev::Disconnected(_))) == 1,
            Duration::from_secs(10),
        )
        .await?;
        assert!(policy.seen.lock().last().unwrap().ctx.we_are_seeding);

        // A file is missing again: not seeding any more.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let both = HashSet::from([0, 1]);
        client
            .session
            .update_only_files(&client.handle, &both)
            .await?;
        wait_for(
            "the second connection",
            || policy.handshakes() == 2,
            Duration::from_secs(10),
        )
        .await?;
        {
            let seen = policy.seen.lock();
            let handshakes: Vec<_> = seen
                .iter()
                .filter(|s| matches!(s.ev, Ev::Handshake { .. }))
                .collect();
            assert_eq!(handshakes.len(), 2);
            assert!(!handshakes[1].ctx.we_are_seeding);
        }

        // The remaining pieces arrive: seeding again.
        timeout(
            Duration::from_secs(20),
            client.handle.wait_until_completed(),
        )
        .await??;
        wait_for(
            "the second connection ended",
            || policy.count(|e| matches!(e, Ev::Disconnected(_))) == 2,
            Duration::from_secs(10),
        )
        .await?;
        assert!(policy.seen.lock().last().unwrap().ctx.we_are_seeding);
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_readmit_without_a_policy_or_refusals_does_nothing() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        assert_eq!(seed.session.readmit_refused_peers(), 0);
        let policy = TestPolicy::new();
        let client = start_client(
            &seed,
            ClientOpts {
                policy: Some(policy.clone()),
                initial_peers: vec![seed.addr()],
                ..Default::default()
            },
        )
        .await?;
        timeout(
            Duration::from_secs(20),
            client.handle.wait_until_completed(),
        )
        .await??;
        assert_eq!(client.session.readmit_refused_peers(), 0);
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Session API
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_disconnect_peers_where() -> anyhow::Result<()> {
    run(async {
        let policy = TestPolicy::new();
        let seed = start_seeder(SeederOpts {
            policy: Some(policy.clone()),
            ..Default::default()
        })
        .await?;
        let mut a = FakePeer::connect(seed.addr(), seed.info_hash, &peer_id(b"-FK0007-")).await?;
        let mut b = FakePeer::connect(seed.addr(), seed.info_hash, &peer_id(b"-FK0008-")).await?;
        wait_for(
            "both connected",
            || policy.handshakes() == 2,
            Duration::from_secs(5),
        )
        .await?;

        let seen = std::sync::Mutex::new(Vec::new());
        // Doesn't match: nobody is disconnected.
        let other: IpAddr = "10.9.8.7".parse()?;
        assert_eq!(seed.session.disconnect_peers_where(|ip| ip == other), 0);
        assert_eq!(
            seed.session.disconnect_peers_where(|ip| {
                seen.lock().unwrap().push(ip);
                false
            }),
            0
        );
        assert_eq!(*seen.lock().unwrap(), vec![LOOPBACK, LOOPBACK]);
        for p in [&mut a, &mut b] {
            let (msgs, closed) = p.collect(Duration::from_millis(500)).await;
            assert!(!closed, "{msgs:?}");
        }

        // Matches: both go.
        assert_eq!(seed.session.disconnect_peers_where(|ip| ip == LOOPBACK), 2);
        a.expect_closed(Duration::from_secs(5)).await?;
        b.expect_closed(Duration::from_secs(5)).await?;
        wait_for(
            "disconnected events",
            || policy.count(|e| matches!(e, Ev::Disconnected(_))) == 2,
            Duration::from_secs(5),
        )
        .await?;
        // There is nobody left.
        assert_eq!(seed.session.disconnect_peers_where(|_| true), 0);
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_without_a_policy_connections_are_unaffected() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let mut peer =
            FakePeer::connect(seed.addr(), seed.info_hash, &peer_id(b"-FK0009-")).await?;
        peer.interested().await?;
        peer.request(2, BLOCK, BLOCK).await?;
        let got = peer.expect_pieces(1, Duration::from_secs(5)).await?;
        assert_eq!(got[0].2, seed.block(2, BLOCK, BLOCK));
        assert_eq!(blocked_incoming(&seed.session), 0);
        assert_eq!(
            seed.session.throttled_upload_bps(),
            Some(crate::DEFAULT_THROTTLED_UPLOAD_BPS)
        );
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Connections that are cut off without a word
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_pausing_the_torrent_reports_the_end_of_its_connections() -> anyhow::Result<()> {
    run(async {
        let policy = TestPolicy::new();
        let seed = start_seeder(SeederOpts {
            policy: Some(policy.clone()),
            ..Default::default()
        })
        .await?;
        let mut peer =
            FakePeer::connect(seed.addr(), seed.info_hash, &peer_id(b"-FK0010-")).await?;
        wait_for(
            "connected",
            || policy.handshakes() == 1,
            Duration::from_secs(5),
        )
        .await?;

        seed.session.pause(&seed.handle).await?;
        peer.expect_closed(Duration::from_secs(5)).await?;
        wait_for(
            "disconnected",
            || policy.count(|e| matches!(e, Ev::Disconnected(_))) == 1,
            Duration::from_secs(5),
        )
        .await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(policy.count(|e| matches!(e, Ev::Disconnected(_))), 1);
        let seen = policy.seen.lock();
        let last = seen.last().unwrap();
        assert_eq!(last.ev, Ev::Disconnected(None));
        assert!(last.ctx.we_are_seeding, "seeding state of the torrent");
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_removing_the_torrent_reports_the_end_of_its_connections() -> anyhow::Result<()> {
    run(async {
        let policy = TestPolicy::new();
        let seed = start_seeder(SeederOpts {
            policy: Some(policy.clone()),
            ..Default::default()
        })
        .await?;
        let mut a = FakePeer::connect(seed.addr(), seed.info_hash, &peer_id(b"-FK0011-")).await?;
        let mut b = FakePeer::connect(seed.addr(), seed.info_hash, &peer_id(b"-FK0012-")).await?;
        // One of them announced something, it is not reported once the task is gone.
        a.bitfield(&[0xf0]).await?;
        wait_for(
            "connected",
            || policy.handshakes() == 2,
            Duration::from_secs(5),
        )
        .await?;

        seed.session
            .delete(crate::api::TorrentIdOrHash::Id(seed.handle.id()), false)
            .await?;
        a.expect_closed(Duration::from_secs(5)).await?;
        b.expect_closed(Duration::from_secs(5)).await?;
        wait_for(
            "disconnected",
            || policy.count(|e| matches!(e, Ev::Disconnected(_))) == 2,
            Duration::from_secs(5),
        )
        .await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(policy.count(|e| matches!(e, Ev::Disconnected(_))), 2);
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Bounded queues
// ---------------------------------------------------------------------------------------------

fn flood(blocks: usize) -> Vec<u8> {
    let mut batch = Vec::with_capacity(blocks * 17);
    for i in 0..u32::try_from(blocks).unwrap_or(u32::MAX) {
        let (piece, begin) = ((i / 2) % PIECES, (i % 2) * BLOCK);
        batch.extend(FakePeer::request_frame(piece, begin, BLOCK));
    }
    batch
}

#[tokio::test(flavor = "multi_thread")]
async fn test_throttled_peer_cannot_queue_unbounded_requests() -> anyhow::Result<()> {
    run(async {
        let policy = TestPolicy::with_rule(|e| match e {
            Ev::Handshake { .. } => PeerVerdict::Throttle,
            _ => PeerVerdict::Allow,
        });
        let seed = start_seeder(SeederOpts {
            policy: Some(policy.clone()),
            ..Default::default()
        })
        .await?;
        let mut peer =
            FakePeer::connect(seed.addr(), seed.info_hash, &peer_id(b"-FK0013-")).await?;
        peer.send_raw(&flood(10_000)).await?;
        wait_for(
            "every request reported",
            || policy.count(|e| matches!(e, Ev::Request { .. })) == 10_000,
            Duration::from_secs(20),
        )
        .await?;
        // While the flood was read, served blocks made room for more requests. What is waiting
        // now is all there is to serve: no more than the queue of a connection holds, though
        // 10 000 requests came in and the rate would allow many more in this time.
        let before = uploaded(&seed);
        tokio::time::sleep(Duration::from_secs(6)).await;
        let served_after = (uploaded(&seed) - before) / BLOCK as u64;
        assert!(
            served_after <= 4,
            "{served_after} blocks served after the flood"
        );
        // The queue frees up as blocks are served: new requests are accepted again.
        // (Blocks may still be served while this reads, hence the two readings of the counter.)
        let uploaded_before_read = uploaded(&seed);
        let (msgs, closed) = peer.collect(Duration::from_secs(1)).await;
        let uploaded_after_read = uploaded(&seed);
        assert!(!closed);
        let received = msgs
            .iter()
            .filter(|m| matches!(m, Wire::Piece { .. }))
            .count() as u64;
        assert!(received * BLOCK as u64 <= uploaded_after_read);
        assert!(received * BLOCK as u64 >= uploaded_before_read);
        peer.request(3, BLOCK, BLOCK).await?;
        peer.expect_pieces(1, Duration::from_secs(5)).await?;
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_throttled_peers_take_turns() -> anyhow::Result<()> {
    run(async {
        let policy = TestPolicy::with_rule(|e| match e {
            Ev::Handshake { .. } => PeerVerdict::Throttle,
            _ => PeerVerdict::Allow,
        });
        let seed = start_seeder(SeederOpts {
            policy: Some(policy.clone()),
            ..Default::default()
        })
        .await?;
        let mut a = FakePeer::connect(seed.addr(), seed.info_hash, &peer_id(b"-FK0014-")).await?;
        let mut b = FakePeer::connect(seed.addr(), seed.info_hash, &peer_id(b"-FK0015-")).await?;
        // The first peer floods the queue before the second one asks for anything.
        a.send_raw(&flood(2_000)).await?;
        wait_for(
            "first flood",
            || policy.count(|e| matches!(e, Ev::Request { .. })) == 2_000,
            Duration::from_secs(10),
        )
        .await?;
        b.send_raw(&flood(2_000)).await?;

        // (Shorter than the 10 s after which a silent peer is dropped.)
        let (got_a, got_b) = tokio::join!(
            a.collect(Duration::from_secs(7)),
            b.collect(Duration::from_secs(7)),
        );
        let pieces = |msgs: &[Wire]| {
            msgs.iter()
                .filter(|m| matches!(m, Wire::Piece { .. }))
                .count()
        };
        assert!(!got_a.1 && !got_b.1);
        // The rate is the clock here, so a slow machine only gets fewer blocks through: what
        // matters is that both are served and that neither is ahead by much.
        let (pa, pb) = (pieces(&got_a.0), pieces(&got_b.0));
        assert!(pa >= 1, "first peer got {pa} blocks");
        assert!(pb >= 1, "second peer got {pb} blocks");
        assert!(pa.abs_diff(pb) <= 6, "unfair: {pa} against {pb}");
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Magnet links
// ---------------------------------------------------------------------------------------------

// Resolving a magnet link dials peers too, to read the metadata from them.
async fn magnet_dials(
    policy: Arc<TestPolicy>,
    seed: &Fixture,
    peer: SocketAddr,
) -> anyhow::Result<Arc<Session>> {
    let dir = TempDir::with_prefix("rqbit_peer_policy_magnet")?;
    let session = Session::new_with_opts(
        dir.path().into(),
        SessionOptions {
            dht: None,
            persistence: None,
            connect: Some(ConnectionOptions::default()),
            ipv4_only: true,
            disable_local_service_discovery: true,
            peer_policy: Some(policy as Arc<dyn PeerPolicy>),
            ..Default::default()
        },
    )
    .await?;
    let magnet = format!("magnet:?xt=urn:btih:{}", seed.info_hash.as_string());
    let s = session.clone();
    // Resolving never finishes: the peer is not a real one.
    tokio::spawn(async move {
        let _dir = dir;
        let _ = s
            .add_torrent(
                AddTorrent::from_url(magnet),
                Some(AddTorrentOptions {
                    initial_peers: Some(vec![peer]),
                    ..Default::default()
                }),
            )
            .await;
    });
    Ok(session)
}

#[tokio::test(flavor = "multi_thread")]
async fn test_refused_peer_is_not_dialed_to_read_magnet_metadata() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let policy = TestPolicy::new();
        policy.set_admit(false);
        let session = magnet_dials(policy.clone(), &seed, listener.local_addr()?).await?;
        wait_for(
            "refusal",
            || blocked_outgoing(&session) == 1,
            Duration::from_secs(5),
        )
        .await?;
        assert!(
            timeout(Duration::from_millis(1500), listener.accept())
                .await
                .is_err(),
            "a refused address was dialed for its metadata"
        );
        assert_eq!(policy.admit_calls(), vec![LOOPBACK]);
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_admitted_peer_is_dialed_to_read_magnet_metadata() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let policy = TestPolicy::new();
        let session = magnet_dials(policy.clone(), &seed, listener.local_addr()?).await?;
        let (_conn, _) = timeout(Duration::from_secs(10), listener.accept())
            .await
            .context("an admitted address was not dialed for its metadata")??;
        assert_eq!(policy.admit_calls(), vec![LOOPBACK]);
        assert_eq!(blocked_outgoing(&session), 0);
        Ok(())
    })
    .await
}
