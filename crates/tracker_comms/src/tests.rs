//! Behaviour of [`TorrentTrackers`] against fake trackers on loopback: a plain TCP server that
//! records every HTTP request and answers with scripted bencode, and a UDP tracker that
//! speaks BEP 15; UDP trackers also through the fake proxies of `librqbit_core`. Everything
//! waits by polling with a deadline.

use std::{
    collections::HashSet,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use futures::StreamExt;
use librqbit_core::{
    hash_id::Id20,
    proxy::{
        ProxyConfig,
        test_util::{FakeProxy, FakeProxyKind, FakeProxyOptions},
    },
};
use parking_lot::Mutex;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{
    TorrentStatsProvider, TorrentTrackers, TrackerAnnounceOptions, TrackerCommsParams,
    TrackerCommsStats, TrackerState, TrackerTimings, UdpTrackerClient,
};

// ---- fake HTTP tracker ----

#[derive(Clone, Debug)]
struct Req {
    query: String,
}

impl Req {
    fn param(&self, key: &str) -> Option<&str> {
        self.query
            .split('&')
            .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
    }
}

enum Reply {
    Body(Vec<u8>),
    Status(u16),
    Hang,
    /// The body, after a while.
    Delayed(Duration, Vec<u8>),
}

type Script = Arc<dyn Fn(&Req, usize) -> Reply + Send + Sync>;

struct FakeHttp {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Req>>>,
    /// The scrape requests, kept apart from the announces.
    scrapes: Arc<Mutex<Vec<Req>>>,
    /// What scrapes are answered with.
    scrape_body: Arc<Mutex<Vec<u8>>>,
}

impl FakeHttp {
    async fn start(script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let scrapes = Arc::new(Mutex::new(Vec::new()));
        let scrape_body = Arc::new(Mutex::new(b"d5:filesdee".to_vec()));
        let (scrapes_in, scrape_body_in) = (scrapes.clone(), scrape_body.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let (recorded, script) = (recorded.clone(), script.clone());
                let (scrapes, scrape_body) = (scrapes_in.clone(), scrape_body_in.clone());
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16 * 1024];
                    let mut n = 0;
                    while !buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf[n..]).await {
                            Ok(0) | Err(_) => return,
                            Ok(m) => n += m,
                        }
                    }
                    let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let target = head
                        .lines()
                        .next()
                        .and_then(|l| l.split(' ').nth(1))
                        .unwrap_or_default();
                    let (path, query) = target.split_once('?').unwrap_or((target, ""));
                    let req = Req {
                        query: query.to_owned(),
                    };
                    let (status, body) = if path.ends_with("/scrape") {
                        scrapes.lock().push(req);
                        (200, scrape_body.lock().clone())
                    } else {
                        let idx = {
                            let mut g = recorded.lock();
                            g.push(req.clone());
                            g.len() - 1
                        };
                        match script(&req, idx) {
                            Reply::Body(b) => (200, b),
                            Reply::Status(s) => (s, Vec::new()),
                            Reply::Hang => futures::future::pending().await,
                            Reply::Delayed(d, b) => {
                                tokio::time::sleep(d).await;
                                (200, b)
                            }
                        }
                    };
                    let head = format!(
                        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                });
            }
        });
        Self {
            addr,
            requests,
            scrapes,
            scrape_body,
        }
    }

    fn ok(body: &'static str) -> Script {
        Arc::new(move |_, _| Reply::Body(body.as_bytes().to_vec()))
    }

    fn url(&self) -> Url {
        format!("http://{}/announce", self.addr).parse().unwrap()
    }

    fn count(&self) -> usize {
        self.requests.lock().len()
    }

    fn events(&self) -> Vec<Option<String>> {
        self.requests
            .lock()
            .iter()
            .map(|r| r.param("event").map(str::to_owned))
            .collect()
    }

    fn count_event(&self, event: &str) -> usize {
        self.events()
            .iter()
            .filter(|e| e.as_deref() == Some(event))
            .count()
    }
}

const PLAIN: &str = "d8:intervali1800e5:peers0:e";

// ---- fake UDP tracker ----

struct FakeUdp {
    addr: SocketAddr,
    /// (event, num_want) of every announce.
    announces: Arc<Mutex<Vec<(u32, i32)>>>,
    /// How many scrapes it got.
    scrapes: Arc<AtomicUsize>,
    /// Where every request it answered came from.
    sources: Arc<Mutex<Vec<SocketAddr>>>,
}

impl FakeUdp {
    async fn start() -> Self {
        Self::start_on("127.0.0.1:0".parse().unwrap(), 7).await
    }

    /// A tracker on `bind` that knows `seeders` seeders.
    async fn start_on(bind: SocketAddr, seeders: u32) -> Self {
        let sock = UdpSocket::bind(bind).await.unwrap();
        let addr = sock.local_addr().unwrap();
        let announces = Arc::new(Mutex::new(Vec::new()));
        let recorded = announces.clone();
        let scrapes = Arc::new(AtomicUsize::new(0));
        let scraped = scrapes.clone();
        let sources = Arc::new(Mutex::new(Vec::new()));
        let seen = sources.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            loop {
                let Ok((n, from)) = sock.recv_from(&mut buf).await else {
                    return;
                };
                let pkt = &buf[..n];
                let Some(action) = pkt.get(8..12) else {
                    continue;
                };
                let Some(tid) = pkt.get(12..16) else {
                    continue;
                };
                let mut out = Vec::new();
                match u32::from_be_bytes(action.try_into().unwrap()) {
                    0 => {
                        out.extend_from_slice(&0u32.to_be_bytes());
                        out.extend_from_slice(tid);
                        out.extend_from_slice(&0x1122334455667788u64.to_be_bytes());
                    }
                    1 if n >= 98 => {
                        let event = u32::from_be_bytes(pkt[80..84].try_into().unwrap());
                        let num_want = i32::from_be_bytes(pkt[92..96].try_into().unwrap());
                        recorded.lock().push((event, num_want));
                        out.extend_from_slice(&1u32.to_be_bytes());
                        out.extend_from_slice(tid);
                        out.extend_from_slice(&1800u32.to_be_bytes());
                        out.extend_from_slice(&3u32.to_be_bytes()); // leechers
                        out.extend_from_slice(&seeders.to_be_bytes());
                    }
                    2 if n >= 36 => {
                        scraped.fetch_add(1, Ordering::SeqCst);
                        out.extend_from_slice(&2u32.to_be_bytes());
                        out.extend_from_slice(tid);
                        out.extend_from_slice(&seeders.to_be_bytes());
                        out.extend_from_slice(&42u32.to_be_bytes()); // completed
                        out.extend_from_slice(&3u32.to_be_bytes()); // leechers
                    }
                    _ => continue,
                }
                seen.lock().push(from);
                let _ = sock.send_to(&out, from).await;
            }
        });
        Self {
            addr,
            announces,
            scrapes,
            sources,
        }
    }

    fn url(&self) -> Url {
        format!("udp://{}/announce", self.addr).parse().unwrap()
    }

    fn events(&self) -> Vec<u32> {
        self.announces.lock().iter().map(|a| a.0).collect()
    }
}

// ---- harness ----

struct Fixed(TrackerCommsStats);

impl TorrentStatsProvider for Fixed {
    fn get(&self) -> TrackerCommsStats {
        self.0
    }
}

fn timings() -> TrackerTimings {
    TrackerTimings {
        retry_min: Duration::from_millis(50),
        retry_max: Duration::from_millis(200),
        reannounce_floor: Duration::from_millis(50),
        request_timeout: Duration::from_secs(5),
        stopped_timeout: Duration::from_millis(300),
    }
}

fn all() -> TrackerAnnounceOptions {
    TrackerAnnounceOptions::default()
}

fn opts(tiers: bool, trackers: bool) -> TrackerAnnounceOptions {
    TrackerAnnounceOptions {
        announce_to_all_tiers: tiers,
        announce_to_all_trackers: trackers,
    }
}

async fn make(
    tiers: Vec<Vec<Url>>,
    options: TrackerAnnounceOptions,
    force: Option<Duration>,
) -> Arc<TorrentTrackers> {
    let n = tiers.len();
    make_with_own(tiers, n, options, force).await
}

/// Like [`make`], but only the first `own` tiers are the torrent's; the rest are the
/// session's.
async fn make_with_own(
    tiers: Vec<Vec<Url>>,
    own: usize,
    options: TrackerAnnounceOptions,
    force: Option<Duration>,
) -> Arc<TorrentTrackers> {
    make_with_timings(tiers, own, options, force, timings()).await
}

async fn make_with_timings(
    tiers: Vec<Vec<Url>>,
    own: usize,
    options: TrackerAnnounceOptions,
    force: Option<Duration>,
    timings: TrackerTimings,
) -> Arc<TorrentTrackers> {
    let udp_client = UdpTrackerClient::new(CancellationToken::new(), None)
        .await
        .unwrap();
    build(tiers, own, options, force, timings, udp_client)
}

fn build(
    tiers: Vec<Vec<Url>>,
    own: usize,
    options: TrackerAnnounceOptions,
    force: Option<Duration>,
    timings: TrackerTimings,
    udp_client: UdpTrackerClient,
) -> Arc<TorrentTrackers> {
    TorrentTrackers::new(
        TrackerCommsParams {
            info_hash: Id20::new([7; 20]),
            peer_id: Id20::new([9; 20]),
            stats: Box::new(Fixed(TrackerCommsStats {
                total_bytes: 100,
                downloaded_bytes: 40,
                ..Default::default()
            })),
            force_interval: force,
            announce_port: 6881,
            reqwest_client: reqwest::Client::new(),
            udp_client,
            options,
            timings,
            enabled: true,
        },
        tiers,
        own,
        false,
    )
}

/// Polls the stream the way the torrent does; the trackers start with its first poll.
fn consume(tt: &Arc<TorrentTrackers>) -> JoinHandle<()> {
    let tt = tt.clone();
    tokio::spawn(async move {
        let mut s = tt.peer_stream().expect("trackers in use");
        while s.next().await.is_some() {}
    })
}

async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

const FAST: Option<Duration> = Some(Duration::from_millis(100));

// ---- events ----

#[tokio::test(flavor = "multi_thread")]
async fn http_event_lifecycle() {
    let t = FakeHttp::start(FakeHttp::ok(PLAIN)).await;
    let tt = make(vec![vec![t.url()]], all(), FAST).await;
    let consumer = consume(&tt);

    eventually("started and periodic announces", || t.count() >= 3).await;
    let events = t.events();
    assert_eq!(events[0].as_deref(), Some("started"));
    assert!(
        events[1..].iter().all(Option::is_none),
        "periodic announces carry no event: {events:?}"
    );
    let first = t.requests.lock()[0].clone();
    assert_eq!(first.param("left"), Some("60"));
    assert_eq!(first.param("compact"), Some("1"));
    assert_eq!(first.param("port"), Some("6881"));

    tt.completed();
    tt.completed();
    eventually("completed", || t.count_event("completed") == 1).await;
    let n = t.count();
    eventually("more announces", || t.count() >= n + 3).await;
    assert_eq!(t.count_event("completed"), 1, "completed exactly once");

    tt.stop();
    eventually("stopped", || t.count_event("stopped") == 1).await;
    let stopped = t
        .requests
        .lock()
        .iter()
        .find(|r| r.param("event") == Some("stopped"))
        .cloned()
        .unwrap();
    assert_eq!(stopped.param("numwant"), Some("0"));
    let after_stop = t.count();
    eventually("the first driver to finish", || consumer.is_finished()).await;

    // Resuming starts again.
    let _consumer = consume(&tt);
    eventually("started after resume", || t.count_event("started") == 2).await;
    assert!(t.count() > after_stop);
    assert_eq!(t.count_event("completed"), 1, "not again after a resume");
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_peer_stream_stops_the_trackers() {
    let t = FakeHttp::start(FakeHttp::ok(PLAIN)).await;
    let tt = make(vec![vec![t.url()]], all(), FAST).await;
    let consumer = consume(&tt);
    eventually("started", || t.count() >= 1).await;
    consumer.abort();
    eventually("stopped", || t.count_event("stopped") == 1).await;
    let n = t.count();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(t.count(), n, "no announces after stopping");
}

#[tokio::test(flavor = "multi_thread")]
async fn stopped_goes_only_to_trackers_that_acknowledged_started() {
    let t = FakeHttp::start(Arc::new(|_, _| Reply::Status(500))).await;
    // No retry within the test, so that `stop` can't land while a `started` is in flight
    // (that one would get a `stopped`, as the tracker may have seen it).
    let tt = make_with_timings(
        vec![vec![t.url()]],
        1,
        all(),
        FAST,
        TrackerTimings {
            retry_min: Duration::from_secs(600),
            retry_max: Duration::from_secs(600),
            ..timings()
        },
    )
    .await;
    let _consumer = consume(&tt);
    eventually("a failed announce", || {
        tt.stats()[0].state == TrackerState::Error
    })
    .await;
    tt.stop();
    tt.wait_stopped().await;
    assert_eq!(t.count_event("stopped"), 0);
    assert_eq!(t.count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn stopped_never_blocks_on_a_hanging_tracker() {
    let t = FakeHttp::start(Arc::new(|r, _| {
        if r.param("event") == Some("stopped") {
            Reply::Hang
        } else {
            Reply::Body(PLAIN.as_bytes().to_vec())
        }
    }))
    .await;
    let tt = make(vec![vec![t.url()]], all(), FAST).await;
    let _consumer = consume(&tt);
    eventually("started", || t.count() >= 1).await;
    let before = Instant::now();
    tt.stop();
    assert!(
        before.elapsed() < Duration::from_millis(100),
        "stop() returns at once"
    );
    eventually("the stopped request arrived", || {
        t.count_event("stopped") == 1
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn udp_event_lifecycle_and_counts() {
    let u = FakeUdp::start().await;
    let tt = make(vec![vec![u.url()]], all(), FAST).await;
    let _consumer = consume(&tt);
    eventually("periodic announces", || u.events().len() >= 3).await;
    let events = u.events();
    assert_eq!(events[0], 2, "started");
    assert!(events[1..].iter().all(|e| *e == 0), "{events:?}");

    let st = tt.stats();
    assert_eq!(st.len(), 1);
    assert_eq!(st[0].state, TrackerState::Working);
    assert_eq!((st[0].seeders, st[0].leechers), (Some(7), Some(3)));

    tt.completed();
    eventually("completed", || u.events().contains(&1)).await;
    tt.stop();
    eventually("stopped", || u.events().contains(&3)).await;
    let ev = u.events();
    assert_eq!(ev.iter().filter(|e| **e == 1).count(), 1);
    let stop_at = ev.iter().position(|e| *e == 3).unwrap();
    assert_eq!(u.announces.lock()[stop_at].1, 0, "numwant of stopped");
}

// ---- what the tracker says ----

#[tokio::test(flavor = "multi_thread")]
async fn tracker_id_is_echoed_and_counts_are_recorded() {
    let t = FakeHttp::start(FakeHttp::ok(
        "d8:completei5e10:incompletei3e8:intervali1800e5:peers0:10:tracker id3:abce",
    ))
    .await;
    let tt = make(vec![vec![t.url()]], all(), FAST).await;
    let _consumer = consume(&tt);
    eventually("two announces", || t.count() >= 2).await;
    let reqs = t.requests.lock().clone();
    assert_eq!(reqs[0].param("trackerid"), None);
    assert_eq!(reqs[1].param("trackerid"), Some("abc"), "{reqs:?}");
    let key = reqs[0].param("key").unwrap();
    assert!(
        reqs.iter().all(|r| r.param("key") == Some(key)),
        "stable key"
    );
    let st = &tt.stats()[0];
    assert_eq!((st.seeders, st.leechers), (Some(5), Some(3)));
    assert_eq!(st.peers_received, Some(0));
    assert!(st.last_announce.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn min_interval_is_honoured() {
    let t = FakeHttp::start(FakeHttp::ok(
        "d8:intervali10e12:min intervali3600e5:peers0:e",
    ))
    .await;
    let tt = make(vec![vec![t.url()]], all(), None).await;
    let _consumer = consume(&tt);
    eventually("an announce", || {
        tt.stats()[0].state == TrackerState::Working
    })
    .await;
    let next = tt.stats()[0].next_announce.expect("scheduled");
    let wait = next.duration_since(SystemTime::now()).unwrap();
    assert!(wait > Duration::from_secs(3000), "waits {wait:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn warning_and_failure_are_surfaced() {
    let t = FakeHttp::start(Arc::new(|_, idx| {
        Reply::Body(if idx == 0 {
            b"d8:intervali1800e5:peers0:15:warning message4:slowe".to_vec()
        } else {
            b"d14:failure reason4:nopee".to_vec()
        })
    }))
    .await;
    let tt = make(vec![vec![t.url()]], all(), FAST).await;
    let _consumer = consume(&tt);
    // The next announce goes out 100 ms later; the warning is there while the first is the
    // last answer.
    eventually("the warning", || {
        let st = &tt.stats()[0];
        st.warning.as_deref() == Some("slow") && st.state == TrackerState::Working
    })
    .await;
    eventually("the failure", || tt.stats()[0].state == TrackerState::Error).await;
    let st = &tt.stats()[0];
    assert!(st.last_error.as_deref().unwrap().contains("nope"));
    assert_eq!(st.warning, None);
}

// ---- tiers ----

async fn http_set(scripts: Vec<Script>) -> Vec<FakeHttp> {
    let mut v = Vec::new();
    for s in scripts {
        v.push(FakeHttp::start(s).await);
    }
    v
}

fn failing() -> Script {
    Arc::new(|_, _| Reply::Status(500))
}

#[tokio::test(flavor = "multi_thread")]
async fn bep12_in_order_fail_over_and_promotion() {
    // Tier 0: a (down) then b; tier 1: c.
    let t = http_set(vec![failing(), FakeHttp::ok(PLAIN), FakeHttp::ok(PLAIN)]).await;
    let (a, b, c) = (&t[0], &t[1], &t[2]);
    let tt = make(
        vec![vec![a.url(), b.url()], vec![c.url()]],
        opts(false, false),
        FAST,
    )
    .await;
    let _consumer = consume(&tt);
    eventually("b answers twice", || b.count() >= 2).await;
    assert!(a.count() >= 1);
    assert_eq!(c.count(), 0, "the second tier is not needed");
    assert_eq!(
        tt.tiers()[0][0],
        b.url(),
        "the tracker that answered moves to the front"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bep12_fails_over_to_the_next_tier() {
    let t = http_set(vec![failing(), FakeHttp::ok(PLAIN)]).await;
    let tt = make(
        vec![vec![t[0].url()], vec![t[1].url()]],
        opts(false, false),
        FAST,
    )
    .await;
    let _consumer = consume(&tt);
    eventually("the second tier answers", || t[1].count() >= 2).await;
    assert!(t[0].count() >= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn default_options_announce_to_everything() {
    let t = http_set(vec![failing(), FakeHttp::ok(PLAIN), FakeHttp::ok(PLAIN)]).await;
    let tt = make(
        vec![vec![t[0].url(), t[1].url()], vec![t[2].url()]],
        all(),
        FAST,
    )
    .await;
    let _consumer = consume(&tt);
    eventually("every tracker is asked", || {
        t.iter().all(|t| t.count() >= 2)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn announce_to_all_tiers_only_uses_one_tracker_per_tier() {
    let t = http_set(vec![
        FakeHttp::ok(PLAIN),
        FakeHttp::ok(PLAIN),
        FakeHttp::ok(PLAIN),
    ])
    .await;
    let tt = make(
        vec![vec![t[0].url(), t[1].url()], vec![t[2].url()]],
        opts(true, false),
        FAST,
    )
    .await;
    let _consumer = consume(&tt);
    eventually("both tiers", || t[2].count() >= 2).await;
    let (first, second) = (tt.tiers()[0][0].clone(), tt.tiers()[0][1].clone());
    let by_url = |u: &Url| t.iter().find(|t| t.url() == *u).unwrap().count();
    assert!(by_url(&first) >= 2);
    assert_eq!(
        by_url(&second),
        0,
        "the second tracker of a working tier idles"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn announce_to_all_trackers_only_uses_the_first_working_tier() {
    let t = http_set(vec![
        FakeHttp::ok(PLAIN),
        FakeHttp::ok(PLAIN),
        FakeHttp::ok(PLAIN),
    ])
    .await;
    let tt = make(
        vec![vec![t[0].url(), t[1].url()], vec![t[2].url()]],
        opts(false, true),
        FAST,
    )
    .await;
    let _consumer = consume(&tt);
    eventually("the first tier", || t[0].count() >= 2 && t[1].count() >= 2).await;
    assert_eq!(t[2].count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_urls_keep_their_first_position() {
    let a: Url = "http://a.example/announce".parse().unwrap();
    let b: Url = "http://b.example/announce".parse().unwrap();
    let tt = make(
        vec![vec![a.clone(), b.clone()], vec![a.clone()], vec![b.clone()]],
        all(),
        None,
    )
    .await;
    let rows = tt.stats();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.iter()
            .map(|r| (r.url.as_str(), r.tier))
            .collect::<Vec<_>>(),
        vec![(a.as_str(), 0), (b.as_str(), 0)]
    );
    assert!(rows.iter().all(|r| r.state == TrackerState::NotContacted));
}

// ---- scrape ----

const ANNOUNCE_WITH_COUNTS: &str = "d8:completei5e10:incompletei3e8:intervali1800e5:peers0:e";

/// The answer of a tracker to a scrape of the torrent the tests use (hash `[7; 20]`).
fn scrape_body(seeders: u32, leechers: u32, downloaded: u32) -> Vec<u8> {
    let mut b = b"d5:filesd20:".to_vec();
    b.extend_from_slice(&[7; 20]);
    b.extend_from_slice(
        format!("d8:completei{seeders}e10:downloadedi{downloaded}e10:incompletei{leechers}eeee")
            .as_bytes(),
    );
    b
}

#[tokio::test(flavor = "multi_thread")]
async fn http_scrape_after_announce_fills_the_counts() {
    let t = FakeHttp::start(FakeHttp::ok(ANNOUNCE_WITH_COUNTS)).await;
    *t.scrape_body.lock() = scrape_body(11, 4, 99);
    let tt = make(vec![vec![t.url()]], all(), None).await;
    let _consumer = consume(&tt);
    eventually("the scrape", || tt.stats()[0].downloaded.is_some()).await;
    let st = &tt.stats()[0];
    assert_eq!(
        (st.seeders, st.leechers, st.downloaded),
        (Some(11), Some(4), Some(99))
    );
    assert_eq!(st.state, TrackerState::Working);
    let query = t.scrapes.lock()[0].clone();
    assert_eq!(query.param("info_hash"), Some("%07".repeat(20).as_str()));
    // The interval is 30 minutes: one announce, one scrape.
    assert_eq!(t.count(), 1);
    assert_eq!(t.scrapes.lock().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn scrape_on_demand_is_rate_limited_but_goes_through() {
    let t = FakeHttp::start(FakeHttp::ok(ANNOUNCE_WITH_COUNTS)).await;
    let tt = make(vec![vec![t.url()]], all(), None).await;
    let _consumer = consume(&tt);
    eventually("the scrape after the announce", || {
        !t.scrapes.lock().is_empty()
    })
    .await;
    *t.scrape_body.lock() = scrape_body(1, 2, 3);
    // Asking again and again is fine: the floor decides when one is made.
    eventually("a scrape on demand", || {
        tt.scrape().unwrap();
        tt.stats()[0].downloaded == Some(3)
    })
    .await;
    assert_eq!(tt.stats()[0].seeders, Some(1));
    assert_eq!(t.count(), 1, "scraping is not announcing");
}

#[tokio::test(flavor = "multi_thread")]
async fn scrape_needs_a_scrapable_url() {
    let t = FakeHttp::start(FakeHttp::ok(PLAIN)).await;
    let url: Url = format!("http://{}/tracker", t.addr).parse().unwrap();
    let tt = make(vec![vec![url]], all(), FAST).await;
    let _consumer = consume(&tt);
    eventually("announces", || t.count() >= 2).await;
    tt.scrape().unwrap();
    let n = t.count();
    eventually("more announces", || t.count() >= n + 2).await;
    assert!(t.scrapes.lock().is_empty());
    assert_eq!(tt.stats()[0].downloaded, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn scrape_skips_trackers_that_are_not_in_use() {
    let t = http_set(vec![FakeHttp::ok(PLAIN), FakeHttp::ok(PLAIN)]).await;
    let tt = make(
        vec![vec![t[0].url()], vec![t[1].url()]],
        opts(false, false),
        FAST,
    )
    .await;
    let _consumer = consume(&tt);
    eventually("a scrape of the first tier", || {
        // The driver starts with the first poll of the stream; until then there is nobody to ask.
        tt.scrape().ok();
        t[0].scrapes.lock().len() >= 2
    })
    .await;
    assert_eq!(t[1].count(), 0);
    assert!(t[1].scrapes.lock().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_scrape_leaves_the_state_alone() {
    let t = FakeHttp::start(FakeHttp::ok(ANNOUNCE_WITH_COUNTS)).await;
    *t.scrape_body.lock() = b"d14:failure reason4:nopee".to_vec();
    let tt = make(vec![vec![t.url()]], all(), FAST).await;
    let _consumer = consume(&tt);
    eventually("the first scrape", || t.scrapes.lock().len() == 1).await;
    // A failed scrape turns the automatic ones off; the announces go on.
    let n = t.count();
    eventually("more announces", || t.count() >= n + 3).await;
    assert_eq!(
        t.scrapes.lock().len(),
        1,
        "no automatic scrape after a failure"
    );
    // The next announce may be in flight (`Updating`): wait for its answer.
    eventually("working", || tt.stats()[0].state == TrackerState::Working).await;
    let st = &tt.stats()[0];
    assert_eq!((st.seeders, st.leechers), (Some(5), Some(3)));
    assert_eq!(st.downloaded, None);
    assert_eq!(st.last_error, None);
    // Asking still works.
    eventually("a scrape on demand", || {
        tt.scrape().unwrap();
        t.scrapes.lock().len() >= 2
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn udp_scrape_fills_the_counts() {
    let u = FakeUdp::start().await;
    let tt = make(vec![vec![u.url()]], all(), None).await;
    let _consumer = consume(&tt);
    eventually("the scrape", || tt.stats()[0].downloaded == Some(42)).await;
    assert!(u.scrapes.load(Ordering::SeqCst) >= 1);
    let st = &tt.stats()[0];
    assert_eq!((st.seeders, st.leechers), (Some(7), Some(3)));
}

/// A host with an IPv4 and an IPv6 address is announced to on both, but it is one row:
/// working if either works, the larger numbers win.
#[tokio::test(flavor = "multi_thread")]
async fn dual_stack_udp_tracker_is_one_row() {
    let v4 = FakeUdp::start_on("127.0.0.1:0".parse().unwrap(), 7).await;
    let Ok(v6) =
        UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], v4.addr.port()))).await
    else {
        eprintln!("skipped: no IPv6 loopback");
        return;
    };
    drop(v6);
    let v6 = FakeUdp::start_on(
        SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], v4.addr.port())),
        9,
    )
    .await;
    let host = format!("localhost:{}", v4.addr.port());
    let Ok(resolved) = tokio::net::lookup_host(host.as_str()).await else {
        eprintln!("skipped: localhost does not resolve");
        return;
    };
    let resolved: Vec<_> = resolved.collect();
    if !(resolved.iter().any(SocketAddr::is_ipv4) && resolved.iter().any(SocketAddr::is_ipv6)) {
        eprintln!("skipped: localhost does not resolve to both families: {resolved:?}");
        return;
    }
    let url: Url = format!("udp://{host}/announce").parse().unwrap();
    let tt = make(vec![vec![url]], all(), None).await;
    let _consumer = consume(&tt);
    eventually("both endpoints announced and scraped", || {
        !v4.announces.lock().is_empty()
            && !v6.announces.lock().is_empty()
            && tt.stats()[0].downloaded.is_some()
    })
    .await;
    let rows = tt.stats();
    assert_eq!(rows.len(), 1, "one row for both endpoints");
    assert_eq!(rows[0].state, TrackerState::Working);
    assert_eq!(rows[0].seeders, Some(9), "the larger of 7 and 9");
    assert_eq!(rows[0].downloaded, Some(42));
}

// A magnet that turns out to be private drops the session's tiers: the ones told `started`
// hear `stopped`, nobody asks them again, not even after a resume, and they leave the
// tracker list.
#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_session_tiers() {
    let own = FakeHttp::start(FakeHttp::ok(PLAIN)).await;
    let session = FakeHttp::start(FakeHttp::ok(PLAIN)).await;
    let tt = make_with_own(vec![vec![own.url()], vec![session.url()]], 1, all(), FAST).await;
    let consumer = consume(&tt);
    eventually("both announced", || {
        own.count_event("started") == 1 && session.count_event("started") == 1
    })
    .await;

    tt.drop_session_tiers();
    eventually("stopped to the session tracker", || {
        session.count_event("stopped") == 1
    })
    .await;
    assert_eq!(own.count_event("stopped"), 0, "the own tracker goes on");
    let rows = tt.stats();
    assert_eq!(
        rows.iter().map(|r| r.url.as_str()).collect::<Vec<_>>(),
        vec![own.url().as_str()]
    );
    let heard = session.count();
    let n = own.count();
    eventually("more announces to the own tracker", || own.count() >= n + 2).await;

    // A pause and a resume do not bring it back.
    tt.stop();
    eventually("stopped to the own tracker", || {
        own.count_event("stopped") == 1
    })
    .await;
    eventually("the first driver to finish", || consumer.is_finished()).await;
    let _consumer = consume(&tt);
    eventually("started again", || own.count_event("started") == 2).await;
    let n = own.count();
    eventually("more announces to the own tracker", || own.count() >= n + 2).await;
    assert_eq!(session.count(), heard, "{:?}", session.events());

    // Dropping again (or with no session tiers left) does nothing.
    tt.drop_session_tiers();
    assert_eq!(tt.stats().len(), 1);
}

// The peers of an announce response come out of the stream.
#[tokio::test(flavor = "multi_thread")]
async fn peers_reach_the_stream() {
    let mut body = b"d8:intervali1800e5:peers12:".to_vec();
    body.extend_from_slice(&[127, 0, 0, 1, 0x1a, 0xe1, 127, 0, 0, 2, 0x1a, 0xe2]);
    body.push(b'e');
    let t = FakeHttp::start(Arc::new(move |_, _| Reply::Body(body.clone()))).await;
    let tt = make(vec![vec![t.url()]], all(), None).await;
    let mut stream = tt.peer_stream().expect("trackers in use");
    let mut got = Vec::new();
    while got.len() < 2 {
        let addr = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("peers in time")
            .expect("the stream goes on");
        got.push(addr);
    }
    got.sort();
    assert_eq!(
        got,
        vec![
            "127.0.0.1:6881".parse::<SocketAddr>().unwrap(),
            "127.0.0.2:6882".parse().unwrap()
        ]
    );
    assert_eq!(tt.stats()[0].peers_received, Some(2));
}

// Intervals far beyond anything sensible are capped instead of overflowing the clocks.
#[tokio::test(flavor = "multi_thread")]
async fn huge_intervals_are_capped() {
    // The largest numbers bencode reads without overflowing its own integer parser.
    let body = "d8:intervali9223372036854775807e12:min intervali9223372036854775807e5:peers0:e";
    for force in [None, FAST] {
        let t = FakeHttp::start(FakeHttp::ok(body)).await;
        let tt = make(vec![vec![t.url()]], all(), force).await;
        let _consumer = consume(&tt);
        let mut next = None;
        eventually("a scheduled announce", || {
            let st = &tt.stats()[0];
            next = st
                .next_announce
                .filter(|_| st.state == TrackerState::Working);
            next.is_some()
        })
        .await;
        let wait = next
            .and_then(|n| n.duration_since(SystemTime::now()).ok())
            .unwrap_or_default();
        assert!(wait <= Duration::from_secs(24 * 3600), "{wait:?}");
        tt.reannounce().unwrap();
        tt.scrape().unwrap();
        // With the forced 100 ms interval the next announce may be in flight.
        assert_ne!(tt.stats()[0].state, TrackerState::Error);
    }
}

// A reannounce goes out at once, to working trackers and to the ones waiting for a retry.
#[tokio::test(flavor = "multi_thread")]
async fn reannounce_goes_now_and_retries_failing_trackers() {
    let t = http_set(vec![FakeHttp::ok(PLAIN), failing()]).await;
    let tt = make_with_timings(
        vec![vec![t[0].url()], vec![t[1].url()]],
        2,
        all(),
        None,
        TrackerTimings {
            retry_min: Duration::from_secs(600),
            retry_max: Duration::from_secs(600),
            ..timings()
        },
    )
    .await;
    let _consumer = consume(&tt);
    eventually("the first announces", || {
        let st = tt.stats();
        st[0].state == TrackerState::Working && st[1].state == TrackerState::Error
    })
    .await;
    assert_eq!((t[0].count(), t[1].count()), (1, 1));

    tt.reannounce().unwrap();
    eventually("both asked again", || {
        t[0].count() == 2 && t[1].count() == 2
    })
    .await;
    // The working one was told `started` already; the failed one never answered it.
    assert_eq!(t[0].events()[1], None);
    assert_eq!(t[1].events()[1].as_deref(), Some("started"));
}

// A `completed` asked for while the `started` (which said something was left) is in flight
// still goes out after it.
#[tokio::test(flavor = "multi_thread")]
async fn completed_while_started_is_in_flight() {
    let t = FakeHttp::start(Arc::new(|_, idx| {
        if idx == 0 {
            Reply::Delayed(Duration::from_millis(300), PLAIN.as_bytes().to_vec())
        } else {
            Reply::Body(PLAIN.as_bytes().to_vec())
        }
    }))
    .await;
    let tt = make(vec![vec![t.url()]], all(), None).await;
    let _consumer = consume(&tt);
    eventually("the started request", || t.count() == 1).await;
    assert_eq!(tt.stats()[0].state, TrackerState::Updating);
    tt.completed();
    eventually("completed", || t.count_event("completed") == 1).await;
    assert_eq!(
        t.events(),
        vec![Some("started".into()), Some("completed".into())]
    );
}

// After a quick pause and resume the tracker hears `stopped` before the next `started`, so it
// does not drop us for a whole interval.
#[tokio::test(flavor = "multi_thread")]
async fn stopped_comes_before_the_next_started() {
    let t = FakeHttp::start(Arc::new(|r, _| {
        let body = PLAIN.as_bytes().to_vec();
        if r.param("event") == Some("stopped") {
            Reply::Delayed(Duration::from_millis(150), body)
        } else {
            Reply::Body(body)
        }
    }))
    .await;
    let tt = make(vec![vec![t.url()]], all(), None).await;
    let consumer = consume(&tt);
    eventually("started", || tt.stats()[0].state == TrackerState::Working).await;
    tt.stop();
    eventually("the first driver to finish", || consumer.is_finished()).await;
    let _consumer = consume(&tt);
    eventually("started again", || t.count_event("started") == 2).await;
    assert_eq!(
        t.events(),
        vec![
            Some("started".into()),
            Some("stopped".into()),
            Some("started".into())
        ]
    );
}

// ---- through a proxy ----

async fn proxied(proxy: &FakeProxy, tracker: Url) -> Arc<TorrentTrackers> {
    let config = ProxyConfig::parse(&proxy.url()).unwrap();
    let udp_client =
        UdpTrackerClient::new_with_proxy(CancellationToken::new(), None, Some(&config))
            .await
            .unwrap();
    build(vec![vec![tracker]], 1, all(), FAST, timings(), udp_client)
}

/// Checks that everything the tracker got came from the proxy's relay sockets.
fn assert_only_relayed(u: &FakeUdp, proxy: &FakeProxy) {
    let relays: HashSet<SocketAddr> = proxy.datagrams().iter().map(|d| d.from).collect();
    let sources = u.sources.lock().clone();
    assert!(!sources.is_empty());
    assert!(
        sources.iter().all(|s| relays.contains(s)),
        "the tracker was reached directly: {sources:?}, relays {relays:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn udp_announce_and_scrape_through_socks5() {
    let proxy = FakeProxy::start(FakeProxyOptions::new(FakeProxyKind::Socks5)).await;
    let u = FakeUdp::start().await;
    let tt = proxied(&proxy, u.url()).await;
    let _consumer = consume(&tt);
    eventually("announces and a scrape", || {
        u.events().len() >= 2 && tt.stats()[0].downloaded == Some(42)
    })
    .await;
    let st = &tt.stats()[0];
    assert_eq!(st.state, TrackerState::Working);
    assert_eq!((st.seeders, st.leechers), (Some(7), Some(3)));
    assert_eq!(u.events()[0], 2, "started");
    assert!(u.scrapes.load(Ordering::SeqCst) >= 1);
    assert_only_relayed(&u, &proxy);
    let target = u.addr.to_string();
    assert!(proxy.datagrams().iter().all(|d| d.target == target));
    assert_eq!(proxy.associations(), 1, "one association for every request");
}

/// Behind `socks5h` the tracker is given to the proxy by name, and its answers are taken also
/// when the relay names it as their source (`ATYP=3`).
#[tokio::test(flavor = "multi_thread")]
async fn socks5h_sends_the_tracker_host_name_to_the_proxy() {
    for name_sources in [false, true] {
        let mut opts = FakeProxyOptions::new(FakeProxyKind::Socks5);
        opts.remote_dns = true;
        opts.name_sources = name_sources;
        let proxy = FakeProxy::start(opts).await;
        let u = FakeUdp::start().await;
        let url: Url = format!("udp://localhost:{}/announce", u.addr.port())
            .parse()
            .unwrap();
        let tt = proxied(&proxy, url).await;
        let _consumer = consume(&tt);
        eventually("an announce and a scrape", || {
            !u.events().is_empty() && tt.stats()[0].downloaded == Some(42)
        })
        .await;
        assert_eq!(tt.stats()[0].state, TrackerState::Working);
        let target = format!("localhost:{}", u.addr.port());
        let datagrams = proxy.datagrams();
        assert!(
            datagrams.iter().all(|d| d.target == target),
            "the host name was resolved locally: {datagrams:?}"
        );
        assert_only_relayed(&u, &proxy);
    }
}

/// SOCKS4 and HTTP proxies can't relay UDP: the tracker is in error and nothing is sent, to
/// the tracker or to the proxy.
#[tokio::test(flavor = "multi_thread")]
async fn udp_tracker_behind_a_tcp_only_proxy_is_an_error_and_sends_nothing() {
    for kind in [FakeProxyKind::Socks4, FakeProxyKind::Http] {
        let proxy = FakeProxy::start(FakeProxyOptions::new(kind)).await;
        let u = FakeUdp::start().await;
        let tt = proxied(&proxy, u.url()).await;
        let _consumer = consume(&tt);
        eventually("the error", || tt.stats()[0].state == TrackerState::Error).await;
        tt.scrape().unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let st = &tt.stats()[0];
        let error = st.last_error.clone().unwrap_or_default();
        assert!(error.contains("cannot relay UDP"), "{kind:?}: {error}");
        assert!(
            u.sources.lock().is_empty(),
            "{kind:?}: the tracker was reached"
        );
        assert_eq!(proxy.accepted(), 0, "{kind:?}: the proxy was contacted");
    }
}

/// While the proxy is down the tracker is in error and gets nothing; when the proxy comes
/// back the association is re-established and the announces go on.
#[tokio::test(flavor = "multi_thread")]
async fn udp_tracker_recovers_when_the_proxy_comes_back() {
    let proxy = FakeProxy::start(FakeProxyOptions::new(FakeProxyKind::Socks5)).await;
    let u = FakeUdp::start().await;
    let tt = proxied(&proxy, u.url()).await;
    let _consumer = consume(&tt);
    eventually("working", || tt.stats()[0].state == TrackerState::Working).await;

    proxy.stop().await;
    eventually("the error", || tt.stats()[0].state == TrackerState::Error).await;
    let before = u.sources.lock().len();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        u.sources.lock().len(),
        before,
        "sent while the proxy was down"
    );

    proxy.restart().await;
    let deadline = Instant::now() + Duration::from_secs(40);
    while !(u.sources.lock().len() > before && tt.stats()[0].state == TrackerState::Working) {
        assert!(Instant::now() < deadline, "no recovery");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_only_relayed(&u, &proxy);
    assert!(
        proxy.associations() >= 2,
        "the association was re-established"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn udp_association_is_rebuilt_when_its_control_connection_drops() {
    let proxy = FakeProxy::start(FakeProxyOptions::new(FakeProxyKind::Socks5)).await;
    let u = FakeUdp::start().await;
    let tt = proxied(&proxy, u.url()).await;
    let _consumer = consume(&tt);
    eventually("working", || tt.stats()[0].state == TrackerState::Working).await;
    proxy.drop_udp_associations();
    let before = u.events().len();
    eventually("announces on a new association", || {
        proxy.associations() >= 2 && u.events().len() > before
    })
    .await;
    assert_only_relayed(&u, &proxy);
}
