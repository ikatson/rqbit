//! Behaviour of [`TorrentTrackers`] against fake trackers on loopback: a plain TCP server that
//! records every HTTP request and answers with scripted bencode, and a UDP tracker that
//! speaks BEP 15. Everything waits by polling with a deadline.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use futures::StreamExt;
use librqbit_core::hash_id::Id20;
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
                let _ = sock.send_to(&out, from).await;
            }
        });
        Self {
            addr,
            announces,
            scrapes,
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
            udp_client: UdpTrackerClient::new(CancellationToken::new(), None)
                .await
                .unwrap(),
            options,
            timings: timings(),
            enabled: true,
        },
        tiers,
        n,
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
    let tt = make(vec![vec![t.url()]], all(), FAST).await;
    let _consumer = consume(&tt);
    eventually("failing announces", || t.count() >= 2).await;
    tt.stop();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(t.count_event("stopped"), 0);
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
    eventually("the warning", || {
        tt.stats()[0].warning.as_deref() == Some("slow")
    })
    .await;
    assert_eq!(tt.stats()[0].state, TrackerState::Working);
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
    eventually("two scrapes", || t.scrapes.lock().len() >= 2).await;
    let st = &tt.stats()[0];
    assert_eq!(st.state, TrackerState::Working);
    assert_eq!((st.seeders, st.leechers), (Some(5), Some(3)));
    assert_eq!(st.downloaded, None);
    assert_eq!(st.last_error, None);
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
