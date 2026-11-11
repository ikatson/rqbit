//! Connection limits: the session-wide cap and each torrent's own, changed while connections are
//! open.
//!
//! Outgoing connections go to listeners that stand in for peers: they accept a connection and
//! hold it without a word, so the client keeps it (connecting) until the test closes it. They
//! count the connections on the accepting side. A connection is taken off the count before the
//! test closes it, so the count never runs ahead of what the client holds; the client never gives
//! up on these connections by itself (the read timeout is long, MSE is off so there is no
//! plaintext redial).

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
    },
    time::Duration,
};

use anyhow::Context;
use bytes::Bytes;
use parking_lot::Mutex;
use tempfile::TempDir;
use tokio::{io::AsyncReadExt, net::TcpListener, sync::Semaphore};
use tokio_util::sync::CancellationToken;

use super::fake_peer::{FakePeer, peer_id, run};
use crate::{
    AddTorrent, AddTorrentOptions, ConnectionOptions, CreateTorrentOptions, DEFAULT_PEER_LIMIT,
    ListenerOptions, PeerConnectionOptions, Session, SessionOptions, SessionPersistenceConfig,
    api::TorrentIdOrHash,
    connection_limit::ConnectionLimit,
    create_torrent,
    spawn_utils::BlockingSpawner,
    tests::test_util::{create_default_random_dir_with_torrents, wait_until},
    torrent_state::ManagedTorrentHandle,
};

const PIECE_LEN: u32 = 32 * 1024;
const PIECES: u32 = 4;
// Long enough that the client never gives up on a silent connection during a test.
const PATIENT: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------------------------------------
// Peers that accept and count
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Count {
    open: AtomicUsize,
    max_open: AtomicUsize,
    accepted: AtomicUsize,
}

impl Count {
    fn open(&self) -> usize {
        self.open.load(SeqCst)
    }

    fn max_open(&self) -> usize {
        self.max_open.load(SeqCst)
    }

    fn accepted(&self) -> usize {
        self.accepted.load(SeqCst)
    }

    /// Starts a new high-water mark from the connections open now.
    fn reset_max(&self) {
        self.max_open.store(self.open(), SeqCst);
    }
}

struct Conn {
    counts: [Arc<Count>; 2],
    counted: AtomicBool,
    close: CancellationToken,
}

impl Conn {
    // Takes the connection off the counts, once, whichever side ends it.
    fn uncount(&self) {
        if self.counted.swap(false, SeqCst) {
            for c in &self.counts {
                c.open.fetch_sub(1, SeqCst);
            }
        }
    }
}

/// The peers of one torrent: a listener each.
struct Swarm {
    addrs: Vec<SocketAddr>,
    count: Arc<Count>,
    conns: Arc<Mutex<Vec<Arc<Conn>>>>,
}

impl Swarm {
    /// `peers` listeners; their connections count in the swarm's own count and in `total`.
    async fn start(peers: usize, total: &Arc<Count>) -> anyhow::Result<Self> {
        let count = Arc::new(Count::default());
        let conns = Arc::new(Mutex::new(Vec::new()));
        let mut addrs = Vec::with_capacity(peers);
        for _ in 0..peers {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
            addrs.push(listener.local_addr()?);
            tokio::spawn(accept(
                listener,
                [count.clone(), total.clone()],
                conns.clone(),
            ));
        }
        Ok(Self {
            addrs,
            count,
            conns,
        })
    }

    /// Closes up to `n` of the open connections from this side; returns how many it closed.
    fn close(&self, n: usize) -> usize {
        let mut conns = self.conns.lock();
        conns.retain(|c| c.counted.load(SeqCst));
        let n = n.min(conns.len());
        for c in conns.drain(..n) {
            c.uncount();
            c.close.cancel();
        }
        n
    }
}

async fn accept(listener: TcpListener, counts: [Arc<Count>; 2], conns: Arc<Mutex<Vec<Arc<Conn>>>>) {
    while let Ok((mut stream, _)) = listener.accept().await {
        for c in &counts {
            let open = c.open.fetch_add(1, SeqCst) + 1;
            c.max_open.fetch_max(open, SeqCst);
            c.accepted.fetch_add(1, SeqCst);
        }
        let conn = Arc::new(Conn {
            counts: counts.clone(),
            counted: AtomicBool::new(true),
            close: CancellationToken::new(),
        });
        conns.lock().push(conn.clone());
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            loop {
                tokio::select! {
                    _ = conn.close.cancelled() => break,
                    r = stream.read(&mut buf) => if matches!(r, Ok(0) | Err(_)) {
                        break;
                    },
                }
            }
            conn.uncount();
        });
    }
}

/// Closes `n` open connections, taken from the swarms in order.
fn close_any(swarms: &[Swarm], mut n: usize) {
    for s in swarms {
        n -= s.close(n);
    }
    assert_eq!(n, 0, "fewer connections open than asked to close");
}

// ---------------------------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------------------------

async fn make_torrent() -> anyhow::Result<Bytes> {
    let dir = create_default_random_dir_with_torrents(
        1,
        (PIECE_LEN * PIECES) as usize,
        Some("rqbit_connlimit_src"),
    );
    let torrent = create_torrent(
        dir.path(),
        CreateTorrentOptions {
            name: None,
            piece_length: Some(PIECE_LEN),
            ..Default::default()
        },
        &BlockingSpawner::new(1),
    )
    .await?;
    torrent.as_bytes()
}

fn loopback_listener() -> ListenerOptions {
    ListenerOptions {
        listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
        ipv4_only: true,
        ..Default::default()
    }
}

fn client_opts(global_peer_limit: Option<usize>, listen: bool) -> SessionOptions {
    SessionOptions {
        dht: None,
        persistence: None,
        listen: listen.then(loopback_listener),
        connect: Some(ConnectionOptions::default()),
        ipv4_only: true,
        disable_local_service_discovery: true,
        global_peer_limit,
        ..Default::default()
    }
}

struct Client {
    session: Arc<Session>,
    dir: TempDir,
    added: AtomicUsize,
}

impl Client {
    async fn start(opts: SessionOptions) -> anyhow::Result<Self> {
        let dir = TempDir::with_prefix("rqbit_connlimit_client")?;
        let session = Session::new_with_opts(dir.path().join("default"), opts).await?;
        Ok(Self {
            session,
            dir,
            added: AtomicUsize::new(0),
        })
    }

    /// A new torrent to download, from `peers`, into its own folder.
    async fn add(
        &self,
        peers: &[SocketAddr],
        peer_limit: Option<usize>,
        read_write_timeout: Duration,
    ) -> anyhow::Result<ManagedTorrentHandle> {
        let bytes = make_torrent().await?;
        let out = self
            .dir
            .path()
            .join(self.added.fetch_add(1, SeqCst).to_string());
        let handle = self
            .session
            .add_torrent(
                AddTorrent::from_bytes(bytes),
                Some(AddTorrentOptions {
                    output_folder: Some(out.to_str().context("path")?.to_owned()),
                    initial_peers: (!peers.is_empty()).then(|| peers.to_vec()),
                    peer_limit,
                    peer_opts: Some(PeerConnectionOptions {
                        read_write_timeout: Some(read_write_timeout),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
            )
            .await?
            .into_handle()
            .context("into_handle")?;
        handle.wait_until_initialized().await?;
        Ok(handle)
    }

    fn addr(&self) -> anyhow::Result<SocketAddr> {
        self.session.listen_addr().context("the client listens")
    }

    fn slots(&self) -> &ConnectionLimit {
        &self.session.stats.peer_slots
    }
}

fn torrent_slots(handle: &ManagedTorrentHandle) -> &ConnectionLimit {
    &handle.shared().options.peer_slots
}

// No permit out, and all of the cap available again.
fn is_released(slots: &ConnectionLimit) -> bool {
    let full = slots.limit().unwrap_or(Semaphore::MAX_PERMITS);
    slots.in_use() == 0 && slots.available() == full
}

async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) -> anyhow::Result<()> {
    wait_until(
        || {
            anyhow::ensure!(cond(), "not yet");
            Ok(())
        },
        Duration::from_secs(20),
    )
    .await
    .with_context(|| format!("waiting for {what}"))
}

async fn wait_released(client: &Client, torrents: &[&ManagedTorrentHandle]) -> anyhow::Result<()> {
    wait_for("all permits back", || {
        is_released(client.slots()) && torrents.iter().all(|t| is_released(torrent_slots(t)))
    })
    .await
}

// What a connection that should be refused looks like: closed before the handshake reply.
async fn refused(addr: SocketAddr, handle: &ManagedTorrentHandle, id: &[u8; 8]) -> bool {
    FakePeer::connect(addr, handle.info_hash(), &peer_id(id))
        .await
        .is_err()
}

// A while in which nothing is expected to happen.
async fn quiet_period() {
    tokio::time::sleep(Duration::from_millis(500)).await;
}

// ---------------------------------------------------------------------------------------------
// Session-wide cap
// ---------------------------------------------------------------------------------------------

// Three torrents with ten peers each share a cap of four connections. The cap is never exceeded,
// a slot that frees up goes to the torrents in turn (each gets connections, however busy the
// others are), raising the cap connects more at once, lowering it disconnects nobody, and no cap
// at all is what it was before there was one.
#[tokio::test(flavor = "multi_thread")]
async fn test_global_cap_is_shared_fairly_and_changes_at_run_time() -> anyhow::Result<()> {
    run(async {
        let total = Arc::new(Count::default());
        let client = Client::start(client_opts(Some(4), false)).await?;
        assert_eq!(client.session.global_peer_limit(), Some(4));
        let mut swarms = Vec::new();
        for _ in 0..3 {
            let swarm = Swarm::start(10, &total).await?;
            client.add(&swarm.addrs, None, PATIENT).await?;
            swarms.push(swarm);
        }
        wait_for("4 connections", || total.open() == 4).await?;
        assert_eq!(client.session.stats_snapshot().peer_connections, 4);
        assert_eq!(client.slots().available(), 0);

        // Connections end one at a time; the freed slot goes to the torrents in turn.
        let accepted = |swarms: &[Swarm]| {
            swarms
                .iter()
                .map(|s| s.count.accepted())
                .collect::<Vec<_>>()
        };
        let mut rounds = 0;
        while swarms.iter().any(|s| s.count.accepted() == 0) {
            rounds += 1;
            anyhow::ensure!(
                rounds <= 6,
                "a torrent got no connection: accepted {:?}",
                accepted(&swarms)
            );
            // Always take the slot from the busiest torrent: it would get it back if it could
            // jump the queue.
            let busiest = swarms
                .iter()
                .max_by_key(|s| s.count.open())
                .context("swarms")?;
            assert_eq!(busiest.close(1), 1);
            wait_for("the freed slot is used again", || total.open() == 4).await?;
        }
        assert!(
            total.max_open() <= 4,
            "{} connections at once with a cap of 4",
            total.max_open()
        );

        // Raising the cap connects more at once.
        client.session.set_global_peer_limit(Some(7));
        assert_eq!(client.session.global_peer_limit(), Some(7));
        wait_for("7 connections after raising the cap", || total.open() == 7).await?;
        assert_eq!(client.session.stats_snapshot().peer_connections, 7);

        // Lowering it disconnects nobody, and no new connection is made until enough have
        // ended.
        total.reset_max();
        client.session.set_global_peer_limit(Some(2));
        quiet_period().await;
        assert_eq!(total.open(), 7, "lowering the cap disconnected peers");
        let accepted_before = total.accepted();
        close_any(&swarms, 5);
        wait_for("5 connections to end", || client.slots().in_use() == 2).await?;
        quiet_period().await;
        assert_eq!(client.slots().available(), 0);
        assert_eq!(total.accepted(), accepted_before, "connected over the cap");
        close_any(&swarms, 1);
        wait_for("a new connection below the cap", || {
            total.accepted() == accepted_before + 1 && total.open() == 2
        })
        .await?;
        assert_eq!(client.slots().in_use(), 2);
        assert!(total.max_open() <= 7);

        // No cap: every peer that is not waiting out a backoff is connected.
        client.session.set_global_peer_limit(None);
        assert_eq!(client.session.global_peer_limit(), None);
        wait_for("more connections than any cap allowed", || total.open() > 7).await?;
        Ok(())
    })
    .await
}

// Incoming connections are refused while the session-wide cap is reached, even when the torrent
// has room of its own, and accepted again once a connection has ended or the cap is raised.
#[tokio::test(flavor = "multi_thread")]
async fn test_incoming_connections_over_the_global_cap_are_refused() -> anyhow::Result<()> {
    run(async {
        let client = Client::start(client_opts(Some(2), true)).await?;
        let a = client.add(&[], None, PATIENT).await?;
        let b = client.add(&[], None, PATIENT).await?;
        let addr = client.addr()?;

        let p1 = FakePeer::connect(addr, a.info_hash(), &peer_id(b"-FK0001-")).await?;
        let _p2 = FakePeer::connect(addr, b.info_hash(), &peer_id(b"-FK0002-")).await?;
        wait_for("2 connections", || client.slots().in_use() == 2).await?;
        assert!(
            refused(addr, &a, b"-FK0003-").await,
            "accepted over the session cap"
        );
        assert_eq!(torrent_slots(&a).available(), DEFAULT_PEER_LIMIT - 1);
        assert_eq!(client.session.stats_snapshot().peer_connections, 2);

        drop(p1);
        wait_for("the connection to end", || client.slots().in_use() == 1).await?;
        let _p3 = FakePeer::connect(addr, a.info_hash(), &peer_id(b"-FK0004-")).await?;
        wait_for("2 connections", || client.slots().in_use() == 2).await?;
        assert!(
            refused(addr, &b, b"-FK0005-").await,
            "accepted over the session cap"
        );

        client.session.set_global_peer_limit(Some(3));
        let _p4 = FakePeer::connect(addr, b.info_hash(), &peer_id(b"-FK0006-")).await?;
        wait_for("3 connections", || client.slots().in_use() == 3).await?;
        Ok(())
    })
    .await
}

// A torrent whose peer adder waits for the full session budget does not lock out the incoming
// connections of the other torrents: they queue for the budget alongside the dialer and get
// their turn.
#[tokio::test(flavor = "multi_thread")]
async fn test_incoming_connection_gets_its_turn_behind_a_waiting_dialer() -> anyhow::Result<()> {
    run(async {
        let total = Arc::new(Count::default());
        let client = Client::start(client_opts(Some(1), true)).await?;
        let a = client.add(&[], None, PATIENT).await?;
        let swarm = Swarm::start(4, &total).await?;
        let b = client.add(&swarm.addrs, None, PATIENT).await?;
        // One connection, and the peer adder holding the torrent's permit for the next peer
        // while it waits for the session's only one.
        wait_for("b's dialer to wait", || {
            total.open() == 1 && torrent_slots(&b).in_use() == 2
        })
        .await?;

        let (addr, info_hash) = (client.addr()?, a.info_hash());
        let incoming =
            tokio::spawn(
                async move { FakePeer::connect(addr, info_hash, &peer_id(b"-FK0001-")).await },
            );
        wait_for("the incoming connection to wait", || {
            torrent_slots(&a).in_use() == 1
        })
        .await?;

        // The first slot that frees up goes to b's dialer, which was first in line...
        assert_eq!(swarm.close(1), 1);
        wait_for("b's dialer to connect", || {
            total.accepted() == 2 && total.open() == 1
        })
        .await?;
        // ...and the next one to the incoming connection, ahead of b's next dial.
        assert_eq!(swarm.close(1), 1);
        let _peer = incoming
            .await
            .context("join")?
            .context("the incoming connection was refused")?;
        assert_eq!(client.slots().in_use(), 1);
        quiet_period().await;
        assert_eq!(total.open(), 0, "connected over the session cap");
        assert_eq!(total.accepted(), 2);
        Ok(())
    })
    .await
}

// The connections that fetch the metadata of a magnet link count against the session-wide cap,
// and give their permits back when the resolution is abandoned.
#[tokio::test(flavor = "multi_thread")]
async fn test_magnet_metadata_connections_count_against_the_global_cap() -> anyhow::Result<()> {
    run(async {
        let total = Arc::new(Count::default());
        let client = Client::start(client_opts(Some(2), false)).await?;
        let swarm = Swarm::start(6, &total).await?;
        let magnet = format!(
            "magnet:?xt=urn:btih:{}",
            librqbit_core::Id20::new([7; 20]).as_string()
        );
        let resolving = tokio::spawn({
            let session = client.session.clone();
            let peers = swarm.addrs.clone();
            async move {
                session
                    .add_torrent(
                        AddTorrent::from_url(magnet),
                        Some(AddTorrentOptions {
                            initial_peers: Some(peers),
                            peer_opts: Some(PeerConnectionOptions {
                                read_write_timeout: Some(PATIENT),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                    )
                    .await
                    .map(drop)
            }
        });
        wait_for("2 metadata connections", || total.open() == 2).await?;
        quiet_period().await;
        assert_eq!(total.max_open(), 2, "connected over the session cap");
        assert_eq!(client.slots().in_use(), 2);

        resolving.abort();
        wait_released(&client, &[]).await?;
        Ok(())
    })
    .await
}
// ---------------------------------------------------------------------------------------------
// Per-torrent limit
// ---------------------------------------------------------------------------------------------

// A torrent's own limit changes while it runs: raised, it connects more at once; lowered, it
// disconnects nobody and waits until enough connections have ended. With a session cap as well,
// the smaller of the two wins.
#[tokio::test(flavor = "multi_thread")]
async fn test_torrent_peer_limit_changes_at_run_time() -> anyhow::Result<()> {
    run(async {
        let total = Arc::new(Count::default());
        let client = Client::start(client_opts(None, false)).await?;
        let swarm = Swarm::start(16, &total).await?;
        let h = client.add(&swarm.addrs, Some(3), PATIENT).await?;
        assert_eq!(h.peer_limit(), Some(3));
        let slots = torrent_slots(&h);

        wait_for("3 connections", || total.open() == 3).await?;
        quiet_period().await;
        assert_eq!(total.max_open(), 3);
        assert_eq!(slots.available(), 0);

        client.session.set_torrent_peer_limit(&h, Some(5)).await?;
        assert_eq!(h.peer_limit(), Some(5));
        wait_for("5 connections after raising", || total.open() == 5).await?;
        quiet_period().await;
        assert_eq!(total.max_open(), 5);

        client.session.set_torrent_peer_limit(&h, Some(2)).await?;
        quiet_period().await;
        assert_eq!(total.open(), 5, "lowering the limit disconnected peers");
        let accepted_before = total.accepted();
        assert_eq!(swarm.close(3), 3);
        wait_for("3 connections to end", || slots.in_use() == 2).await?;
        quiet_period().await;
        assert_eq!(slots.available(), 0);
        assert_eq!(
            total.accepted(),
            accepted_before,
            "connected over the limit"
        );
        assert_eq!(swarm.close(1), 1);
        wait_for("a new connection below the limit", || {
            total.accepted() == accepted_before + 1 && total.open() == 2
        })
        .await?;

        // The session cap is smaller: it wins.
        client.session.set_global_peer_limit(Some(4));
        client.session.set_torrent_peer_limit(&h, Some(6)).await?;
        total.reset_max();
        wait_for("4 connections under the session cap", || total.open() == 4).await?;
        quiet_period().await;
        assert_eq!(total.max_open(), 4);
        assert!(
            slots.has_room(),
            "the torrent's own limit is not what holds it back"
        );
        // The torrent's limit is smaller: it wins.
        client.session.set_global_peer_limit(Some(10));
        wait_for("6 connections under the torrent limit", || {
            total.open() == 6
        })
        .await?;
        quiet_period().await;
        assert_eq!(total.max_open(), 6);
        assert_eq!(client.slots().available(), 4);

        // `None` is the session default again.
        client.session.set_torrent_peer_limit(&h, None).await?;
        assert_eq!(h.peer_limit(), None);
        assert_eq!(slots.limit(), Some(DEFAULT_PEER_LIMIT));
        Ok(())
    })
    .await
}

// The torrent's own limit is kept across pause / resume and a session restart.
#[tokio::test(flavor = "multi_thread")]
async fn test_torrent_peer_limit_survives_pause_and_restart() -> anyhow::Result<()> {
    run(async {
        let root = TempDir::with_prefix("rqbit_connlimit_persist")?;
        let persistence = || SessionOptions {
            dht: None,
            persistence: Some(SessionPersistenceConfig::Json {
                folder: Some(root.path().join("session")),
            }),
            listen: None,
            disable_local_service_discovery: true,
            ..Default::default()
        };
        let bytes = make_torrent().await?;

        let info_hash = {
            let session = Session::new_with_opts(root.path().join("out"), persistence()).await?;
            let h = session
                .add_torrent(
                    AddTorrent::from_bytes(bytes),
                    Some(AddTorrentOptions {
                        paused: true,
                        ..Default::default()
                    }),
                )
                .await?
                .into_handle()
                .context("into_handle")?;
            h.wait_until_initialized().await?;
            assert_eq!(h.peer_limit(), None);
            assert_eq!(torrent_slots(&h).limit(), Some(DEFAULT_PEER_LIMIT));

            session.set_torrent_peer_limit(&h, Some(7)).await?;
            assert_eq!(h.peer_limit(), Some(7));
            session.unpause(&h).await?;
            assert_eq!(h.peer_limit(), Some(7));
            assert_eq!(torrent_slots(&h).limit(), Some(7));
            session.pause(&h).await?;
            assert_eq!(torrent_slots(&h).limit(), Some(7));
            let info_hash = h.info_hash();
            session.stop().await;
            info_hash
        };

        let session = Session::new_with_opts(root.path().join("out"), persistence()).await?;
        let h = session
            .get(TorrentIdOrHash::Hash(info_hash))
            .context("the torrent was not restored")?;
        assert_eq!(h.peer_limit(), Some(7));
        assert_eq!(torrent_slots(&h).limit(), Some(7));
        session.stop().await;
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Permit accounting
// ---------------------------------------------------------------------------------------------

// However a connection ends, its permits go back: the torrent's and the session's counters
// return to zero with all of their budget available.
#[tokio::test(flavor = "multi_thread")]
async fn test_permits_come_back_on_every_exit_path() -> anyhow::Result<()> {
    run(async {
        let total = Arc::new(Count::default());
        let client = Client::start(client_opts(Some(8), true)).await?;
        let addr = client.addr()?;
        let a = client.add(&[], None, Duration::from_secs(2)).await?;
        let connect = |id: &'static [u8; 8]| {
            let info_hash = a.info_hash();
            async move { FakePeer::connect(addr, info_hash, &peer_id(id)).await }
        };

        // The peer closes.
        let p = connect(b"-FK0001-").await?;
        wait_for("the connection", || client.slots().in_use() == 1).await?;
        drop(p);
        wait_released(&client, &[&a]).await.context("peer closed")?;

        // The read timeout: a peer that unchoked the client, has nothing, and says nothing.
        let mut p = connect(b"-FK0002-").await?;
        p.unchoke().await?;
        p.expect_closed(Duration::from_secs(15)).await?;
        wait_released(&client, &[&a])
            .await
            .context("read timeout")?;

        // Pause, with outgoing and incoming connections.
        let swarm = Swarm::start(3, &total).await?;
        let b = client.add(&swarm.addrs, None, PATIENT).await?;
        wait_for("3 outgoing connections", || total.open() == 3).await?;
        let _p = FakePeer::connect(addr, b.info_hash(), &peer_id(b"-FK0004-")).await?;
        wait_for("4 connections", || client.slots().in_use() == 4).await?;
        client.session.pause(&b).await?;
        wait_released(&client, &[&a, &b]).await.context("pause")?;
        wait_for("the peers to see the connections end", || total.open() == 0).await?;

        // Delete.
        let swarm = Swarm::start(3, &total).await?;
        let c = client.add(&swarm.addrs, None, PATIENT).await?;
        wait_for("3 outgoing connections", || total.open() == 3).await?;
        client
            .session
            .delete(TorrentIdOrHash::Hash(c.info_hash()), false)
            .await?;
        wait_released(&client, &[&a, &b, &c])
            .await
            .context("delete")?;
        wait_for("the peers to see the connections end", || total.open() == 0).await?;

        // Session stop.
        let swarm = Swarm::start(3, &total).await?;
        let d = client.add(&swarm.addrs, None, PATIENT).await?;
        wait_for("3 outgoing connections", || total.open() == 3).await?;
        let _p = connect(b"-FK0005-").await?;
        wait_for("4 connections", || client.slots().in_use() == 4).await?;
        client.session.stop().await;
        wait_released(&client, &[&a, &b, &c, &d])
            .await
            .context("session stop")?;
        wait_for("the peers to see the connections end", || total.open() == 0).await?;
        Ok(())
    })
    .await
}
