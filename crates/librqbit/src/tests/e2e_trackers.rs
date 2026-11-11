//! What a real session tells the trackers of its torrents, seen from a fake HTTP tracker on
//! loopback that records every request.

// The fake tracker is shared with the interop tests.
#![allow(dead_code)]

use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;
use parking_lot::Mutex;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
    time::timeout,
};

use crate::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ConnectionOptions, Session, SessionOptions,
    TrackerState,
    api::TorrentIdOrHash,
    tests::fake_peer::{SeederOpts, run, start_seeder},
    torrent_state::ManagedTorrentHandle,
};

const WAIT: Duration = Duration::from_secs(30);

/// One announce the fake tracker got: the query string, percent-decoded to bytes.
#[derive(Clone, Debug)]
pub(super) struct Announce {
    pub(super) params: Vec<(String, Vec<u8>)>,
}

impl Announce {
    pub(super) fn param(&self, key: &str) -> Option<&[u8]> {
        self.params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_slice())
    }

    /// The `event` of the announce; `None` for a periodic one.
    pub(super) fn event(&self) -> Option<String> {
        self.param("event")
            .map(|e| String::from_utf8_lossy(e).into_owned())
    }

    /// The `left` of the announce.
    pub(super) fn left(&self) -> Option<u64> {
        std::str::from_utf8(self.param("left")?).ok()?.parse().ok()
    }
}

fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match (
            b[i],
            b.get(i + 1).copied().and_then(hex),
            b.get(i + 2).copied().and_then(hex),
        ) {
            (b'%', Some(h), Some(l)) => {
                out.push(u8::try_from(h * 16 + l).unwrap_or_default());
                i += 3;
            }
            (b'+', ..) => {
                out.push(b' ');
                i += 1;
            }
            (c, ..) => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// An HTTP tracker that answers every announce with no peers and a long interval.
pub(super) struct FakeTracker {
    pub(super) addr: SocketAddr,
    announces: Arc<Mutex<Vec<Announce>>>,
    task: JoinHandle<()>,
}

impl Drop for FakeTracker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeTracker {
    pub(super) async fn start() -> anyhow::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let announces = Arc::new(Mutex::new(Vec::new()));
        let recorded = announces.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let recorded = recorded.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&buf).into_owned();
                    let target = head
                        .lines()
                        .next()
                        .and_then(|l| l.split(' ').nth(1))
                        .unwrap_or_default();
                    let (path, query) = target.split_once('?').unwrap_or((target, ""));
                    if path.ends_with("/announce") {
                        let params = query
                            .split('&')
                            .filter_map(|kv| kv.split_once('='))
                            .map(|(k, v)| (k.to_owned(), percent_decode(v)))
                            .collect();
                        recorded.lock().push(Announce { params });
                    }
                    let body = b"d8:intervali1800e5:peers0:e";
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(body).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        Ok(Self {
            addr,
            announces,
            task,
        })
    }

    /// The announce URL of this tracker.
    pub(super) fn url(&self) -> String {
        format!("http://{}/announce", self.addr)
    }

    pub(super) fn announces(&self) -> Vec<Announce> {
        self.announces.lock().clone()
    }

    /// The `event` of every announce so far, in order.
    pub(super) fn events(&self) -> Vec<Option<String>> {
        self.announces().iter().map(Announce::event).collect()
    }

    /// Waits until `n` announces have been recorded.
    pub(super) async fn wait_for_announces(
        &self,
        n: usize,
        within: Duration,
    ) -> anyhow::Result<()> {
        let deadline = Instant::now() + within;
        while self.announces.lock().len() < n {
            if Instant::now() > deadline {
                anyhow::bail!(
                    "timed out waiting for {n} announces, got {:?}",
                    self.events()
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(())
    }
}

/// A session that only talks to the trackers of its torrents.
pub(super) async fn tracker_session(dir: &std::path::Path) -> anyhow::Result<Arc<Session>> {
    Session::new_with_opts(
        dir.into(),
        SessionOptions {
            dht: None,
            persistence: None,
            listen: None,
            connect: Some(ConnectionOptions::default()),
            ipv4_only: true,
            disable_local_service_discovery: true,
            ..Default::default()
        },
    )
    .await
    .context("error creating session")
}

fn some(e: &str) -> Option<String> {
    Some(e.to_owned())
}

async fn add_with_tracker(
    session: &Arc<Session>,
    torrent: bytes::Bytes,
    tracker: &FakeTracker,
    output_folder: &std::path::Path,
    initial_peers: Option<Vec<SocketAddr>>,
) -> anyhow::Result<ManagedTorrentHandle> {
    session
        .add_torrent(
            AddTorrent::from_bytes(torrent),
            Some(AddTorrentOptions {
                paused: false,
                overwrite: true,
                output_folder: Some(output_folder.to_str().context("path")?.to_owned()),
                trackers: Some(vec![tracker.url()]),
                initial_peers,
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .context("into_handle")
}

// started on add, completed once when the download ends, stopped on pause, started again on
// resume (and no second completed: the torrent was complete), stopped on delete.
#[tokio::test(flavor = "multi_thread")]
async fn test_tracker_events_follow_the_torrent_lifecycle() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let tracker = FakeTracker::start().await?;
        let dir = tempfile::tempdir()?;
        let session = tracker_session(dir.path()).await?;
        let handle = add_with_tracker(
            &session,
            seed.torrent_bytes.clone(),
            &tracker,
            dir.path(),
            Some(vec![seed.addr()]),
        )
        .await?;

        timeout(WAIT, handle.wait_until_completed())
            .await
            .context("download timed out")??;
        tracker.wait_for_announces(2, WAIT).await?;
        assert_eq!(tracker.events(), vec![some("started"), some("completed")]);

        session.pause(&handle).await?;
        tracker.wait_for_announces(3, WAIT).await?;
        assert_eq!(tracker.events()[2], some("stopped"));

        session.unpause(&handle).await?;
        tracker.wait_for_announces(4, WAIT).await?;
        assert_eq!(tracker.events()[3], some("started"));
        let stopped = tracker.announces()[2].clone();
        assert_eq!(stopped.param("numwant"), Some(&b"0"[..]));

        session
            .delete(TorrentIdOrHash::Id(handle.id()), false)
            .await?;
        tracker.wait_for_announces(5, WAIT).await?;
        assert_eq!(
            tracker.events(),
            vec![
                some("started"),
                some("completed"),
                some("stopped"),
                some("started"),
                some("stopped")
            ]
        );
        Ok(())
    })
    .await
}

// A torrent that is complete when it is added only ever says started and stopped.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_torrent_added_complete_never_sends_completed() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let tracker = FakeTracker::start().await?;
        let dir = tempfile::tempdir()?;
        let session = tracker_session(dir.path()).await?;
        let handle = add_with_tracker(
            &session,
            seed.torrent_bytes.clone(),
            &tracker,
            seed._dir.path(),
            None,
        )
        .await?;
        timeout(WAIT, handle.wait_until_completed())
            .await
            .context("the torrent did not turn out complete")??;
        tracker.wait_for_announces(1, WAIT).await?;
        let left = tracker.announces()[0].left();
        assert_eq!(left, Some(0), "a complete torrent has nothing left");

        session.pause(&handle).await?;
        tracker.wait_for_announces(2, WAIT).await?;
        session.unpause(&handle).await?;
        tracker.wait_for_announces(3, WAIT).await?;
        assert_eq!(
            tracker.events(),
            vec![some("started"), some("stopped"), some("started")]
        );
        Ok(())
    })
    .await
}

// What `ManagedTorrent::trackers` shows for a live torrent, a paused one, and what a
// reannounce does.
#[tokio::test(flavor = "multi_thread")]
async fn test_tracker_stats_follow_the_announces() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let tracker = FakeTracker::start().await?;
        let dir = tempfile::tempdir()?;
        let session = tracker_session(dir.path()).await?;
        let handle = add_with_tracker(
            &session,
            seed.torrent_bytes.clone(),
            &tracker,
            seed._dir.path(),
            None,
        )
        .await?;
        tracker.wait_for_announces(1, WAIT).await?;

        let deadline = Instant::now() + WAIT;
        loop {
            let rows = handle.trackers();
            if rows.len() == 1 && rows[0].state == TrackerState::Working {
                assert_eq!(rows[0].url, tracker.url());
                assert_eq!(rows[0].tier, 0);
                assert!(rows[0].last_announce.is_some());
                assert!(rows[0].next_announce.is_some());
                assert_eq!(rows[0].peers_received, Some(0));
                break;
            }
            anyhow::ensure!(Instant::now() < deadline, "tracker never working: {rows:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // The state of a paused torrent is the last known one.
        session.pause(&handle).await?;
        let rows = handle.trackers();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].last_announce.is_some());

        // A torrent that is not announcing cannot be asked to announce.
        assert!(session.reannounce(&handle).is_err());
        session.unpause(&handle).await?;
        Ok(())
    })
    .await
}

/// Waits until `f` holds, polling.
async fn eventually(what: &str, mut f: impl FnMut() -> bool) -> anyhow::Result<()> {
    let deadline = Instant::now() + WAIT;
    while !f() {
        anyhow::ensure!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

/// How many announces `tracker` got with `event`.
fn count_event(tracker: &FakeTracker, event: &str) -> usize {
    tracker
        .events()
        .iter()
        .filter(|e| e.as_deref() == Some(event))
        .count()
}

/// A copy of `torrent` with `private` set (BEP 27) and `announce` as its only tracker. Its
/// info hash differs, its content is the same.
fn private_copy(torrent: &[u8], announce: &str) -> anyhow::Result<bytes::Bytes> {
    let parsed = librqbit_core::torrent_metainfo::torrent_from_bytes(torrent)?;
    let info = parsed.info.raw_bytes.as_ref();
    // "private" sorts after every key of a single-file info dict ("length", "name",
    // "piece length", "pieces"), so it goes right before the dict ends.
    let body = info.strip_suffix(b"e").context("info is not a dict")?;
    let mut out = format!("d8:announce{}:{announce}4:info", announce.len()).into_bytes();
    out.extend_from_slice(body);
    out.extend_from_slice(b"7:privatei1ee");
    out.push(b'e');
    Ok(out.into())
}

/// A session that only talks to trackers, with `trackers` as its own tracker list
/// (`SessionOptions::trackers`).
async fn session_with_trackers(
    dir: &std::path::Path,
    trackers: &[&FakeTracker],
) -> anyhow::Result<Arc<Session>> {
    Session::new_with_opts(
        dir.into(),
        SessionOptions {
            dht: None,
            persistence: None,
            listen: None,
            connect: Some(ConnectionOptions::default()),
            ipv4_only: true,
            disable_local_service_discovery: true,
            trackers: trackers
                .iter()
                .map(|t| t.url().parse())
                .collect::<Result<_, _>>()?,
            ..Default::default()
        },
    )
    .await
    .context("error creating session")
}

// Adding a torrent that runs already, again (by file or by magnet) or as a list-only preview,
// must not tell the trackers that the running one stopped: they can't tell the two apart.
#[tokio::test(flavor = "multi_thread")]
async fn test_adding_a_running_torrent_again_sends_no_stopped() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let tracker = FakeTracker::start().await?;
        let dir = tempfile::tempdir()?;
        let session = tracker_session(dir.path()).await?;
        let handle = add_with_tracker(
            &session,
            seed.torrent_bytes.clone(),
            &tracker,
            seed._dir.path(),
            None,
        )
        .await?;
        tracker.wait_for_announces(1, WAIT).await?;

        let again = add_with_tracker(
            &session,
            seed.torrent_bytes.clone(),
            &tracker,
            dir.path(),
            None,
        )
        .await?;
        assert_eq!(again.id(), handle.id());
        let magnet = format!(
            "magnet:?xt=urn:btih:{}&tr={}",
            seed.info_hash.as_string(),
            tracker.url()
        );
        let res = session
            .add_torrent(AddTorrent::from_url(&magnet), None)
            .await?;
        assert!(matches!(res, AddTorrentResponse::AlreadyManaged(..)));
        // The preview has to look for the metadata, so it does announce.
        let res = session
            .add_torrent(
                AddTorrent::from_url(&magnet),
                Some(AddTorrentOptions {
                    list_only: true,
                    initial_peers: Some(vec![seed.addr.context("seeder listens")?]),
                    ..Default::default()
                }),
            )
            .await?;
        assert!(matches!(res, AddTorrentResponse::ListOnly(..)));

        session.pause(&handle).await?;
        eventually("the stopped of the pause", || {
            count_event(&tracker, "stopped") >= 1
        })
        .await?;
        // Everything before the pause is out by now: the pause waits for nothing, but a
        // stopped of the preview would have been sent before the preview returned.
        assert_eq!(
            count_event(&tracker, "stopped"),
            1,
            "only the pause says stopped: {:?}",
            tracker.events()
        );
        assert_eq!(tracker.events().last(), Some(&some("stopped")));
        Ok(())
    })
    .await
}

// A private torrent added from a file keeps its own trackers and never gets the session's.
#[tokio::test(flavor = "multi_thread")]
async fn test_private_torrent_file_gets_no_session_trackers() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let own = FakeTracker::start().await?;
        let sessions = FakeTracker::start().await?;
        let dir = tempfile::tempdir()?;
        let session = session_with_trackers(dir.path(), &[&sessions]).await?;
        let handle = session
            .add_torrent(
                AddTorrent::from_bytes(private_copy(&seed.torrent_bytes, &own.url())?),
                Some(AddTorrentOptions {
                    paused: false,
                    overwrite: true,
                    output_folder: Some(seed._dir.path().to_str().context("path")?.to_owned()),
                    ..Default::default()
                }),
            )
            .await?
            .into_handle()
            .context("into_handle")?;
        own.wait_for_announces(1, WAIT).await?;
        let rows = handle.trackers();
        assert_eq!(
            rows.iter().map(|r| r.url.as_str()).collect::<Vec<_>>(),
            vec![own.url()]
        );
        session.pause(&handle).await?;
        own.wait_for_announces(2, WAIT).await?;
        assert!(sessions.events().is_empty(), "{:?}", sessions.events());
        Ok(())
    })
    .await
}

// Stopping the session gives the stopped announces their time before the session (and with
// it the UDP tracker client) goes away.
#[tokio::test(flavor = "multi_thread")]
async fn test_session_stop_delivers_stopped() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let tracker = FakeTracker::start().await?;
        let dir = tempfile::tempdir()?;
        let session = tracker_session(dir.path()).await?;
        add_with_tracker(
            &session,
            seed.torrent_bytes.clone(),
            &tracker,
            seed._dir.path(),
            None,
        )
        .await?;
        tracker.wait_for_announces(1, WAIT).await?;
        session.stop().await;
        // No waiting here: it is out by the time stop() returns.
        assert_eq!(tracker.events(), vec![some("started"), some("stopped")]);
        Ok(())
    })
    .await
}
