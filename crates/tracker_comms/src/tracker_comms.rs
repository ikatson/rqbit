//! Talking to the trackers of one torrent.
//!
//! Everything lives in [`TorrentTrackers`]: one object per torrent, created when the torrent
//! is added and alive as long as the torrent is. It owns the tier list and the state of every
//! tracker, and it is driven by explicit calls instead of by "the peer stream got dropped":
//!
//! - [`TorrentTrackers::peer_stream`] gives the stream of peers found by the trackers. The
//!   first poll of that stream starts the torrent's announcing (the `started` event).
//! - [`TorrentTrackers::completed`] is called when the download finishes while running.
//! - [`TorrentTrackers::stop`] is called when the torrent is paused, deleted, or the session
//!   shuts down (the `stopped` event).
//! - [`TorrentTrackers::reannounce`] and [`TorrentTrackers::scrape`] are the manual triggers.
//!
//! While running, one task (the driver) owns the schedule: it decides every time which
//! trackers are in use under the BEP 12 tier rules, and runs their announces concurrently.

use std::collections::VecDeque;
use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, bail};
use futures::future::{BoxFuture, Shared};
use futures::stream::{BoxStream, FuturesUnordered};
use futures::{FutureExt, StreamExt};
use parking_lot::Mutex;
use serde_derive::Serialize;
use tokio::sync::mpsc;
use tracing::{Instrument, debug, debug_span, trace, trace_span};
use url::Url;

use crate::scrape::{self, ScrapeCounts};
use crate::tiers::{self, Availability};
use crate::tracker_comms_http::{self, TrackerRequestEvent};
use crate::tracker_comms_udp::{self, UdpTrackerClient};
use librqbit_core::{hash_id::Id20, proxy::TargetAddr};

pub use crate::tiers::TrackerAnnounceOptions;

/// Responses of HTTP trackers are never bigger than this; a bigger one is an error.
const MAX_HTTP_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// How often a tracker is asked again when it gives no interval at all.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(1800);
/// Trackers that ask for less than this get this.
const MIN_INTERVAL: Duration = Duration::from_secs(5);
/// Trackers that ask for more than this get this. Without a cap a tracker could make the
/// time arithmetic overflow, and a day is longer than any tracker means seriously.
const MAX_INTERVAL: Duration = Duration::from_secs(24 * 3600);
/// Peers the trackers returned that the torrent did not take yet; more are dropped. The
/// trackers give more on the next announce.
const MAX_PENDING_PEERS: usize = 10_000;
/// Texts from the trackers (warnings, failure reasons) are kept up to this many bytes.
const MAX_TEXT_LEN: usize = 512;
/// A `tracker id` longer than this is not echoed back.
const MAX_TRACKER_ID_LEN: usize = 1024;

#[derive(Default, Clone, Copy, Debug)]
pub enum TrackerCommsStatsState {
    #[default]
    None,
    Initializing,
    Paused,
    Live,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct TrackerCommsStats {
    pub uploaded_bytes: u64,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub torrent_state: TrackerCommsStatsState,
}

impl TrackerCommsStats {
    pub fn get_left_to_download_bytes(&self) -> u64 {
        let total = self.total_bytes;
        let down = self.downloaded_bytes;
        if total >= down {
            return total - down;
        }
        0
    }

    pub fn is_completed(&self) -> bool {
        self.downloaded_bytes >= self.total_bytes
    }
}

pub trait TorrentStatsProvider: Send + Sync {
    fn get(&self) -> TrackerCommsStats;
}

impl TorrentStatsProvider for () {
    fn get(&self) -> TrackerCommsStats {
        Default::default()
    }
}

/// What the torrent knows about one of its trackers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum TrackerState {
    /// No request was made to this tracker yet (or it is not in use).
    NotContacted,
    /// A request is in flight.
    Updating,
    /// The last announce worked.
    Working,
    /// The last announce failed.
    Error,
}

/// The state of one tracker of a torrent, as reported to users.
#[derive(Clone, Debug, Serialize)]
pub struct TrackerStats {
    pub url: String,
    /// Index of the tier the tracker is in.
    pub tier: usize,
    pub state: TrackerState,
    pub seeders: Option<u32>,
    pub leechers: Option<u32>,
    /// How many times the torrent was downloaded completely, as the tracker counts it.
    pub downloaded: Option<u32>,
    /// Peers in the last announce response.
    pub peers_received: Option<u32>,
    pub last_announce: Option<SystemTime>,
    pub next_announce: Option<SystemTime>,
    pub last_error: Option<String>,
    pub warning: Option<String>,
}

/// Timings of the tracker machinery. The defaults are what clients use; they are
/// configurable so that tests do not need to wait for minutes.
#[derive(Clone, Copy, Debug)]
pub struct TrackerTimings {
    /// The first delay before an announce is retried after it failed; it doubles on every
    /// failure.
    pub retry_min: Duration,
    /// The longest delay before an announce is retried.
    pub retry_max: Duration,
    /// A tracker is not announced to on request more often than this (or its `min interval`,
    /// when that is longer).
    pub reannounce_floor: Duration,
    /// How long a request to a tracker may take.
    pub request_timeout: Duration,
    /// How long the `stopped` announce to a tracker may take.
    pub stopped_timeout: Duration,
}

impl Default for TrackerTimings {
    fn default() -> Self {
        Self {
            retry_min: Duration::from_secs(10),
            retry_max: Duration::from_secs(600),
            reannounce_floor: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
            stopped_timeout: Duration::from_secs(3),
        }
    }
}

/// Everything [`TorrentTrackers`] needs to talk to the trackers.
pub struct TrackerCommsParams {
    pub info_hash: Id20,
    pub peer_id: Id20,
    pub stats: Box<dyn TorrentStatsProvider>,
    pub force_interval: Option<Duration>,
    /// This MUST be set as trackers don't work with 0 port.
    pub announce_port: u16,
    pub reqwest_client: reqwest::Client,
    pub udp_client: UdpTrackerClient,
    pub options: TrackerAnnounceOptions,
    pub timings: TrackerTimings,
    /// When false, the trackers are kept and reported, but never contacted.
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug)]
enum UdpTrackerResolveResult {
    One(SocketAddr),
    Two(SocketAddrV4, SocketAddrV6),
}

async fn udp_tracker_to_socket_addrs(
    host: url::Host<&str>,
    port: u16,
) -> anyhow::Result<UdpTrackerResolveResult> {
    let res = match host {
        url::Host::Domain(name) => {
            // Use the first IPv4 and the first IPv6 addresses only.

            let mut v4: Option<SocketAddrV4> = None;
            let mut v6: Option<SocketAddrV6> = None;
            for addr in tokio::net::lookup_host((name, port))
                .await
                .with_context(|| format!("error looking up hostname {name}"))?
            {
                match (v4, v6, addr) {
                    (None, _, SocketAddr::V4(addr)) => v4 = Some(addr),
                    (_, None, SocketAddr::V6(addr)) => v6 = Some(addr),
                    _ => continue,
                }
            }
            let res = match (v4, v6) {
                (Some(v4), Some(v6)) => UdpTrackerResolveResult::Two(v4, v6),
                (Some(v4), None) => UdpTrackerResolveResult::One(v4.into()),
                (None, Some(v6)) => UdpTrackerResolveResult::One(v6.into()),
                _ => anyhow::bail!("zero addresses returned looking up {name}"),
            };
            trace!(?res, "resolved");
            res
        }
        url::Host::Ipv4(addr) => UdpTrackerResolveResult::One((addr, port).into()),
        url::Host::Ipv6(addr) => UdpTrackerResolveResult::One((addr, port).into()),
    };
    Ok(res)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Http,
    Udp,
    Unsupported,
}

#[derive(Debug)]
enum Command {
    Stop,
    Completed,
    Reannounce,
    Scrape,
}

struct Tracker {
    url: Url,
    kind: Kind,

    // What is reported.
    /// Only ever `NotContacted`, `Working` or `Error`: `Updating` is derived from `in_flight`.
    state: TrackerState,
    seeders: Option<u32>,
    leechers: Option<u32>,
    downloaded: Option<u32>,
    peers_received: Option<u32>,
    last_announce: Option<SystemTime>,
    last_error: Option<String>,
    warning: Option<String>,

    // What the scheduler needs.
    /// When the next announce is due, or when the retry is due after a failure. None = now.
    next_at: Option<Instant>,
    /// When the last request was made.
    last_attempt: Option<Instant>,
    min_interval: Option<Duration>,
    fails: u32,
    in_flight: bool,
    /// The next announce must carry `started`.
    needs_started: bool,
    /// A `started` announce was answered, so the tracker knows about us and wants a `stopped`.
    started_acked: bool,
    /// The next announce must carry `completed`.
    pending_completed: bool,
    tracker_id: Option<Vec<u8>>,
    /// The addresses of a UDP tracker as they were last resolved.
    resolved: Option<UdpTrackerResolveResult>,
    /// Never to be used again: unsupported, or its code panicked.
    disabled: bool,
    /// The tracker can be asked for scrape (BEP 48 / BEP 15).
    scrapable: bool,
    scrape_in_flight: bool,
    /// A scrape is due; it goes out as soon as the tracker is in use.
    scrape_wanted: bool,
    /// Scrapes go out by themselves after announces. Off once a scrape failed, as the tracker
    /// then most likely can't be scraped (asking on demand still works).
    auto_scrape: bool,
    last_scrape: Option<Instant>,
}

impl Tracker {
    fn new(url: Url) -> Self {
        let kind = match url.scheme() {
            "http" | "https" => Kind::Http,
            "udp" => Kind::Udp,
            _ => Kind::Unsupported,
        };
        let unsupported = kind == Kind::Unsupported;
        let scrapable = match kind {
            Kind::Http => scrape::is_scrapable(&url),
            Kind::Udp => true,
            Kind::Unsupported => false,
        };
        Self {
            last_error: unsupported.then(|| format!("unsupported tracker scheme {}", url.scheme())),
            state: if unsupported {
                TrackerState::Error
            } else {
                TrackerState::NotContacted
            },
            disabled: unsupported,
            url,
            kind,
            seeders: None,
            leechers: None,
            downloaded: None,
            peers_received: None,
            last_announce: None,
            warning: None,
            next_at: None,
            last_attempt: None,
            min_interval: None,
            fails: 0,
            in_flight: false,
            needs_started: true,
            started_acked: false,
            pending_completed: false,
            tracker_id: None,
            resolved: None,
            auto_scrape: scrapable,
            scrapable,
            scrape_in_flight: false,
            scrape_wanted: false,
            last_scrape: None,
        }
    }

    fn begin_run(&mut self) {
        self.state = TrackerState::NotContacted;
        self.next_at = None;
        self.fails = 0;
        self.in_flight = false;
        self.needs_started = true;
        self.started_acked = false;
        self.pending_completed = false;
        self.scrape_in_flight = false;
        self.scrape_wanted = false;
    }

    fn availability(&self, now: Instant) -> Availability {
        if self.disabled {
            return Availability::Disabled;
        }
        match self.state {
            TrackerState::Working => Availability::Working,
            TrackerState::Error if self.next_at.is_some_and(|n| n > now) => Availability::Waiting,
            _ => Availability::Pending,
        }
    }

    fn event(&self) -> Option<TrackerRequestEvent> {
        if self.needs_started {
            Some(TrackerRequestEvent::Started)
        } else if self.pending_completed {
            Some(TrackerRequestEvent::Completed)
        } else {
            None
        }
    }

    fn reannounce_floor(&self, timings: &TrackerTimings) -> Duration {
        timings
            .reannounce_floor
            .max(self.min_interval.unwrap_or_default())
    }

    /// When a forced announce may go out at the earliest: once the floor has passed since the
    /// last request.
    fn earliest_reannounce(&self, timings: &TrackerTimings, now: Instant) -> Instant {
        self.last_attempt
            .and_then(|l| l.checked_add(self.reannounce_floor(timings)))
            .unwrap_or(now)
            .max(now)
    }

    /// The `stopped` announce for this tracker, if it knows about us: it acknowledged a
    /// `started`, or a `started` is in flight and may well have arrived.
    fn stopped_job(&self, id: usize, stats: Option<TrackerCommsStats>) -> Option<Job> {
        let knows_us = self.started_acked || (self.in_flight && self.needs_started);
        (knows_us && !self.disabled).then(|| Job {
            id,
            url: self.url.clone(),
            kind: self.kind,
            event: Some(TrackerRequestEvent::Stopped),
            tracker_id: self.tracker_id.clone(),
            resolved: self.resolved,
            no_peers: true,
            stats,
        })
    }
}

struct DriverHandle {
    cmd_tx: mpsc::UnboundedSender<Command>,
    generation: u64,
}

struct Inner {
    /// All the trackers, in the order they were given. Indices into this are stable.
    trackers: Vec<Tracker>,
    /// The tiers, as indices into `trackers`; the order inside a tier changes when a tracker
    /// answers.
    tiers: Vec<Vec<usize>>,
    driver: Option<DriverHandle>,
    generation: u64,
    /// The `stopped` announces of the last stop, ready when they are done. The next run waits
    /// for them, so that a tracker never gets a `started` before the `stopped` before it.
    stopping: Option<Shared<BoxFuture<'static, ()>>>,
    /// `completed` was sent (or the torrent was complete when it started): never again.
    completed_sent: bool,
    /// See [`TorrentTrackers::suppress_stopped`].
    stopped_suppressed: bool,
}

/// A request to a tracker.
struct Job {
    id: usize,
    url: Url,
    kind: Kind,
    event: Option<TrackerRequestEvent>,
    tracker_id: Option<Vec<u8>>,
    resolved: Option<UdpTrackerResolveResult>,
    /// Asks for no peers (the `stopped` event).
    no_peers: bool,
    /// What to report; read from the provider when None.
    stats: Option<TrackerCommsStats>,
}

/// A scrape request to a tracker.
struct ScrapeJob {
    id: usize,
    url: Url,
    kind: Kind,
    resolved: Option<UdpTrackerResolveResult>,
}

struct ScrapeResult {
    result: anyhow::Result<ScrapeCounts>,
    resolved: Option<UdpTrackerResolveResult>,
}

#[derive(Default)]
struct Outcome {
    interval: Option<Duration>,
    min_interval: Option<Duration>,
    seeders: Option<u32>,
    leechers: Option<u32>,
    downloaded: Option<u32>,
    addrs: Vec<SocketAddr>,
    /// The `left` the request said.
    left: u64,
    warning: Option<String>,
    tracker_id: Option<Vec<u8>>,
}

struct JobResult {
    result: anyhow::Result<Outcome>,
    resolved: Option<UdpTrackerResolveResult>,
    panicked: bool,
}

impl JobResult {
    fn failed(e: anyhow::Error) -> Self {
        Self {
            result: Err(e),
            resolved: None,
            panicked: false,
        }
    }
}

type JobFuture = BoxFuture<'static, (usize, Option<TrackerRequestEvent>, JobResult)>;
type ScrapeFuture = BoxFuture<'static, (usize, ScrapeResult)>;

/// A count from a tracker. Negative ones are nonsense and count as zero.
fn count(n: i64) -> u32 {
    u32::try_from(n.max(0)).unwrap_or(u32::MAX)
}

/// An interval a tracker asked for, capped.
fn tracker_interval(secs: u64) -> Duration {
    Duration::from_secs(secs).min(MAX_INTERVAL)
}

/// A text from a tracker, cut to [`MAX_TEXT_LEN`] bytes so that a tracker can't make us keep
/// (and copy on every [`TorrentTrackers::stats`]) megabytes of it.
fn bounded_text(mut s: String) -> String {
    if s.len() > MAX_TEXT_LEN {
        let mut end = MAX_TEXT_LEN;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push('…');
    }
    s
}

async fn read_body_limited(mut response: reqwest::Response) -> anyhow::Result<Vec<u8>> {
    let mut body = Vec::new();
    // `without_url`: the URL carries the passkey, the key and the tracker id, and the error
    // ends up in `TrackerStats::last_error`.
    while let Some(chunk) = response.chunk().await.map_err(|e| e.without_url())? {
        if body.len() + chunk.len() > MAX_HTTP_RESPONSE_BYTES {
            bail!("tracker response is bigger than {MAX_HTTP_RESPONSE_BYTES} bytes");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// The trackers of one torrent. See the module documentation.
pub struct TorrentTrackers {
    info_hash: Id20,
    peer_id: Id20,
    stats: Box<dyn TorrentStatsProvider>,
    force_interval: Option<Duration>,
    announce_port: u16,
    reqwest_client: reqwest::Client,
    udp_client: UdpTrackerClient,
    options: TrackerAnnounceOptions,
    timings: TrackerTimings,
    enabled: bool,
    key: u32,
    /// How many of the first tiers belong to the torrent (and not to the session).
    own_tiers: usize,
    inner: Mutex<Inner>,
}

impl TorrentTrackers {
    /// `tiers` is the tier list, the first `own_tiers` of them being the torrent's own (those
    /// are what is worth saving). Empty tiers are dropped, and the tracker URLs that appear
    /// more than once only stay where they first are. With `shuffle`, every tier is shuffled
    /// once (BEP 12).
    pub fn new(
        params: TrackerCommsParams,
        tiers: Vec<Vec<Url>>,
        own_tiers: usize,
        shuffle: bool,
    ) -> Arc<Self> {
        use rand::seq::SliceRandom;

        let mut seen = std::collections::HashSet::new();
        let mut trackers = Vec::new();
        let mut tier_idx = Vec::new();
        let mut own = 0;
        for (t, tier) in tiers.into_iter().enumerate() {
            let mut idx = Vec::new();
            for url in tier {
                if !seen.insert(url.clone()) {
                    continue;
                }
                idx.push(trackers.len());
                trackers.push(Tracker::new(url));
            }
            if idx.is_empty() {
                continue;
            }
            if shuffle {
                idx.shuffle(&mut rand::rng());
            }
            if t < own_tiers {
                own += 1;
            }
            tier_idx.push(idx);
        }
        Arc::new(Self {
            info_hash: params.info_hash,
            peer_id: params.peer_id,
            stats: params.stats,
            force_interval: params.force_interval,
            announce_port: params.announce_port,
            reqwest_client: params.reqwest_client,
            udp_client: params.udp_client,
            options: params.options,
            timings: params.timings,
            enabled: params.enabled,
            key: rand::random(),
            own_tiers: own,
            inner: Mutex::new(Inner {
                trackers,
                tiers: tier_idx,
                driver: None,
                generation: 0,
                stopping: None,
                completed_sent: false,
                stopped_suppressed: false,
            }),
        })
    }

    /// The torrent's own tiers (without the trackers of the session), in their current order.
    pub fn own_tiers(&self) -> Vec<Vec<Url>> {
        let g = self.inner.lock();
        g.tiers
            .iter()
            .take(self.own_tiers)
            .map(|tier| tier.iter().map(|&i| g.trackers[i].url.clone()).collect())
            .collect()
    }

    /// Every tier, in their current order.
    pub fn tiers(&self) -> Vec<Vec<Url>> {
        let g = self.inner.lock();
        g.tiers
            .iter()
            .map(|tier| tier.iter().map(|&i| g.trackers[i].url.clone()).collect())
            .collect()
    }

    /// The stream of the peers the trackers return. The trackers are started when it is first
    /// polled (and stopped when it is dropped), or None when there is nothing to talk to.
    pub fn peer_stream(self: &Arc<Self>) -> Option<BoxStream<'static, SocketAddr>> {
        if !self.enabled {
            debug!(info_hash=?self.info_hash, "trackers are disabled");
            return None;
        }
        if self.inner.lock().trackers.iter().all(|t| t.disabled) {
            debug!(info_hash=?self.info_hash, "trackers list is empty");
            return None;
        }
        let this = self.clone();
        Some(
            futures::stream::once(async move { this.start() })
                .flatten()
                .boxed(),
        )
    }

    fn start(self: &Arc<Self>) -> BoxStream<'static, SocketAddr> {
        let (peer_tx, mut peer_rx) = mpsc::channel::<SocketAddr>(16);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let stats = self.stats_for_stop();
        {
            let mut g = self.inner.lock();
            // A running driver (it should not be) is replaced.
            self.stop_locked(&mut g, stats);
            g.generation += 1;
            let generation = g.generation;
            g.driver = Some(DriverHandle { cmd_tx, generation });
            for t in g.trackers.iter_mut().filter(|t| !t.disabled) {
                t.begin_run();
            }
            let previous_stop = g.stopping.clone();
            tokio::spawn(
                self.clone()
                    .run_driver(generation, cmd_rx, peer_tx, previous_stop),
            );
        }
        async_stream::stream! {
            while let Some(addr) = peer_rx.recv().await {
                yield addr;
            }
        }
        .boxed()
    }

    /// The torrent is paused, deleted, or the session stops: stops announcing, and sends
    /// `stopped` to the trackers that know about us from a detached task, each bounded in
    /// time (see [`Self::wait_stopped`]).
    pub fn stop(self: &Arc<Self>) {
        let stats = self.stats_for_stop();
        let mut g = self.inner.lock();
        self.stop_locked(&mut g, stats);
    }

    /// Resolves when the `stopped` announces sent so far are done. They are bounded in time
    /// by themselves; this is for a caller that must not go away before they are out, such as
    /// a session that shuts down (the UDP tracker client stops with it).
    pub fn wait_stopped(&self) -> BoxFuture<'static, ()> {
        match self.inner.lock().stopping.clone() {
            Some(stopping) => stopping.boxed(),
            None => futures::future::ready(()).boxed(),
        }
    }

    /// From now on, stopping sends no `stopped`. For an instance that only looked for peers
    /// on behalf of a torrent that another instance already runs (a duplicate add, or a
    /// list-only preview of it): the trackers can't tell the two apart, so its `stopped`
    /// would take the running torrent out of the swarm.
    pub fn suppress_stopped(&self) {
        self.inner.lock().stopped_suppressed = true;
    }

    /// Forgets the tiers after the torrent's own (the trackers the session added), for a
    /// torrent added by magnet that turned out to be private (BEP 27) once its metadata
    /// arrived. The ones that were told `started` get a `stopped`.
    pub fn drop_session_tiers(self: &Arc<Self>) {
        let stats = self.stats_for_stop();
        let mut g = self.inner.lock();
        if g.tiers.len() <= self.own_tiers {
            return;
        }
        let dropped: Vec<usize> = g.tiers.drain(self.own_tiers..).flatten().collect();
        let mut jobs = Vec::new();
        for id in dropped {
            let t = &mut g.trackers[id];
            jobs.extend(t.stopped_job(id, stats));
            t.started_acked = false;
            t.disabled = true;
        }
        if !g.stopped_suppressed {
            self.spawn_stopped(&mut g, jobs);
        }
    }

    /// The numbers for the `stopped` announces, taken when the torrent stops (a deleted
    /// torrent has none afterwards). Read before `inner` is locked, and only when there is a
    /// driver to stop: the provider takes the torrent's locks, and the torrent calls
    /// [`Self::completed`] with them held, so `inner` must never be held while calling it.
    fn stats_for_stop(&self) -> Option<TrackerCommsStats> {
        let running = self.inner.lock().driver.is_some();
        running.then(|| self.stats.get())
    }

    /// `stats` comes from [`Self::stats_for_stop`]; when None (the driver started after it
    /// was read), the `stopped` announces read the provider when they go out.
    fn stop_locked(self: &Arc<Self>, g: &mut Inner, stats: Option<TrackerCommsStats>) {
        let Some(driver) = g.driver.take() else {
            return;
        };
        // A driver that has finished already has nothing to stop.
        let _ = driver.cmd_tx.send(Command::Stop);
        let mut jobs = Vec::new();
        for (id, t) in g.trackers.iter_mut().enumerate() {
            jobs.extend(t.stopped_job(id, stats));
            t.started_acked = false;
            t.in_flight = false;
            t.next_at = None;
            t.scrape_in_flight = false;
            t.scrape_wanted = false;
        }
        if !g.stopped_suppressed {
            self.spawn_stopped(g, jobs);
        }
    }

    /// Sends `stopped` announces from a detached task, so that whoever stops the torrent
    /// never waits for a dead tracker, and remembers them in `stopping`.
    fn spawn_stopped(self: &Arc<Self>, g: &mut Inner, jobs: Vec<Job>) {
        if jobs.is_empty() {
            return;
        }
        let this = self.clone();
        let timeout = self.timings.stopped_timeout;
        let task = tokio::spawn(
            async move {
                futures::future::join_all(
                    jobs.into_iter().map(|job| this.job_future(job, timeout)),
                )
                .await;
            }
            .instrument(debug_span!(parent: None, "tracker_stopped", info_hash = ?self.info_hash)),
        );
        // Stops that are still going on are waited for as well; finished ones are let go, so
        // that the chain does not grow with every pause.
        let previous = g.stopping.take().filter(|s| s.peek().is_none());
        g.stopping = Some(
            async move {
                if let Err(e) = task.await {
                    debug!("the stopped announces failed: {e:#}");
                }
                if let Some(previous) = previous {
                    previous.await;
                }
            }
            .boxed()
            .shared(),
        );
    }

    /// The download finished while the torrent was running. Sends `completed`, once for the
    /// life of the torrent.
    pub fn completed(&self) {
        let mut g = self.inner.lock();
        if g.completed_sent {
            return;
        }
        // Without a driver (the torrent is stopping) nobody would send it: it is not marked
        // as sent then.
        let accepted = g
            .driver
            .as_ref()
            .is_some_and(|d| d.cmd_tx.send(Command::Completed).is_ok());
        if accepted {
            g.completed_sent = true;
        }
    }

    /// Announces to the trackers in use now, without waiting for the interval, and retries
    /// the ones that failed without waiting for their backoff, but never more often than the
    /// reannounce floor of the tracker.
    pub fn reannounce(&self) -> anyhow::Result<()> {
        match &self.inner.lock().driver {
            Some(d) => d
                .cmd_tx
                .send(Command::Reannounce)
                .map_err(|_| anyhow::anyhow!("the trackers task has finished")),
            None => bail!("the torrent is not announcing to its trackers"),
        }
    }

    /// Asks the trackers in use for their numbers (BEP 48 / BEP 15 scrape), not more often
    /// than the reannounce floor of the tracker. The result shows up in [`Self::stats`].
    pub fn scrape(&self) -> anyhow::Result<()> {
        match &self.inner.lock().driver {
            Some(d) => d
                .cmd_tx
                .send(Command::Scrape)
                .map_err(|_| anyhow::anyhow!("the trackers task has finished")),
            None => bail!("the torrent is not announcing to its trackers"),
        }
    }

    /// The state of every tracker, by tier and by position in the tier.
    pub fn stats(&self) -> Vec<TrackerStats> {
        let g = self.inner.lock();
        let now = Instant::now();
        let now_sys = SystemTime::now();
        let running = g.driver.is_some();
        let mut out = Vec::new();
        for (tier, ids) in g.tiers.iter().enumerate() {
            for &i in ids {
                let t = &g.trackers[i];
                out.push(TrackerStats {
                    url: t.url.to_string(),
                    tier,
                    state: if t.in_flight {
                        TrackerState::Updating
                    } else {
                        t.state
                    },
                    seeders: t.seeders,
                    leechers: t.leechers,
                    downloaded: t.downloaded,
                    peers_received: t.peers_received,
                    last_announce: t.last_announce,
                    next_announce: match (running, t.next_at) {
                        (true, Some(n)) if !t.in_flight && !t.disabled => {
                            now_sys.checked_add(n.saturating_duration_since(now))
                        }
                        _ => None,
                    },
                    last_error: t.last_error.clone(),
                    warning: t.warning.clone(),
                });
            }
        }
        out
    }

    // ---- the driver ----

    async fn run_driver(
        self: Arc<Self>,
        generation: u64,
        mut cmd_rx: mpsc::UnboundedReceiver<Command>,
        peer_tx: mpsc::Sender<SocketAddr>,
        previous_stop: Option<Shared<BoxFuture<'static, ()>>>,
    ) {
        // The `stopped` of the last run goes out before the `started` of this one: a tracker
        // that gets them the other way round forgets us until the next announce.
        if let Some(previous) = previous_stop
            && tokio::time::timeout(self.timings.stopped_timeout, previous)
                .await
                .is_err()
        {
            debug!(info_hash = ?self.info_hash, "the stopped announces of the last run are late, starting anyway");
        }
        let mut in_flight: FuturesUnordered<JobFuture> = FuturesUnordered::new();
        let mut scrapes: FuturesUnordered<ScrapeFuture> = FuturesUnordered::new();
        // Peers the trackers returned, waiting for the torrent to take them. Handing them over
        // is not part of a request, so that a torrent that does not take peers for a while
        // (it checks its files) does not make the requests time out.
        let mut pending: VecDeque<SocketAddr> = VecDeque::new();
        loop {
            let Some((jobs, scrape_jobs, wake)) = self.plan(generation) else {
                return;
            };
            for job in jobs {
                in_flight.push(self.job_future(job, self.timings.request_timeout));
            }
            for job in scrape_jobs {
                scrapes.push(self.scrape_future(job, self.timings.request_timeout));
            }
            let sleep = async {
                match wake {
                    Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
                    None => futures::future::pending().await,
                }
            };
            tokio::select! {
                _ = peer_tx.closed() => {
                    self.on_peers_closed(generation);
                    return;
                }
                permit = peer_tx.reserve(), if !pending.is_empty() => match permit {
                    Ok(permit) => {
                        if let Some(addr) = pending.pop_front() {
                            permit.send(addr);
                        }
                        // Whatever else fits goes too, without a trip through the planner.
                        while let Some(&addr) = pending.front() {
                            if peer_tx.try_send(addr).is_err() {
                                break;
                            }
                            pending.pop_front();
                        }
                    }
                    Err(_) => {
                        self.on_peers_closed(generation);
                        return;
                    }
                },
                cmd = cmd_rx.recv() => match cmd {
                    None | Some(Command::Stop) => return,
                    Some(cmd) => self.handle_command(generation, cmd),
                },
                Some((id, event, result)) = in_flight.next(), if !in_flight.is_empty() => {
                    let addrs = self.apply(generation, id, event, result);
                    let room = MAX_PENDING_PEERS.saturating_sub(pending.len());
                    pending.extend(addrs.into_iter().take(room));
                }
                Some((id, result)) = scrapes.next(), if !scrapes.is_empty() => {
                    self.apply_scrape(generation, id, result);
                }
                _ = sleep => {}
            }
        }
    }

    /// Nobody listens to the peers: the torrent went away without telling us.
    fn on_peers_closed(self: &Arc<Self>, generation: u64) {
        let stats = self.stats_for_stop();
        let mut g = self.inner.lock();
        if g.driver
            .as_ref()
            .is_some_and(|d| d.generation == generation)
        {
            self.stop_locked(&mut g, stats);
        }
    }

    /// Chooses what to request now. None when this driver is not the current one.
    fn plan(&self, generation: u64) -> Option<(Vec<Job>, Vec<ScrapeJob>, Option<Instant>)> {
        let mut g = self.inner.lock();
        if g.driver.as_ref().map(|d| d.generation) != Some(generation) {
            return None;
        }
        let now = Instant::now();
        let ids = self.in_use(&g, now);
        let mut jobs = Vec::new();
        let mut scrapes = Vec::new();
        for &id in &ids {
            let t = &mut g.trackers[id];
            if t.in_flight || t.next_at.is_some_and(|n| n > now) {
                continue;
            }
            t.in_flight = true;
            t.last_attempt = Some(now);
            jobs.push(Job {
                id,
                url: t.url.clone(),
                kind: t.kind,
                event: t.event(),
                tracker_id: t.tracker_id.clone(),
                resolved: t.resolved,
                no_peers: false,
                stats: None,
            });
        }
        // Scrapes are only made to trackers that are in use right now.
        for &id in &ids {
            let t = &mut g.trackers[id];
            if t.scrape_wanted && !t.scrape_in_flight && !t.disabled {
                t.scrape_wanted = false;
                t.scrape_in_flight = true;
                t.last_scrape = Some(now);
                scrapes.push(ScrapeJob {
                    id,
                    url: t.url.clone(),
                    kind: t.kind,
                    resolved: t.resolved,
                });
            }
        }
        let wake = g
            .trackers
            .iter()
            .filter(|t| !t.disabled && !t.in_flight)
            .filter_map(|t| t.next_at)
            .filter(|n| *n > now)
            .min();
        Some((jobs, scrapes, wake))
    }

    /// The indices of the trackers in use under the tier rules.
    fn in_use(&self, g: &Inner, now: Instant) -> Vec<usize> {
        let avail: Vec<Vec<Availability>> = g
            .tiers
            .iter()
            .map(|tier| {
                tier.iter()
                    .map(|&i| g.trackers[i].availability(now))
                    .collect()
            })
            .collect();
        tiers::in_use(&avail, self.options)
            .into_iter()
            .filter_map(|(t, p)| g.tiers.get(t)?.get(p).copied())
            .collect()
    }

    fn handle_command(&self, generation: u64, cmd: Command) {
        let mut g = self.inner.lock();
        if g.driver.as_ref().map(|d| d.generation) != Some(generation) {
            return;
        }
        let now = Instant::now();
        match cmd {
            Command::Stop => {}
            Command::Completed => {
                // Every tracker gets it, including the ones whose `started` is in flight or
                // still to be sent: `apply` drops it only after a `started` that already said
                // nothing is left.
                for t in g.trackers.iter_mut().filter(|t| !t.disabled) {
                    t.pending_completed = true;
                    if !t.in_flight && t.state == TrackerState::Working {
                        t.next_at = Some(now);
                    }
                }
            }
            Command::Reannounce => {
                // The trackers that failed and wait for their retry are retried too, as
                // libtorrent's `force_reannounce` does: they are the ones that need it most.
                for t in g.trackers.iter_mut() {
                    if t.availability(now) == Availability::Waiting {
                        t.next_at = Some(t.earliest_reannounce(&self.timings, now));
                    }
                }
                for id in self.in_use(&g, now) {
                    let t = &mut g.trackers[id];
                    if !t.in_flight {
                        t.next_at = Some(t.earliest_reannounce(&self.timings, now));
                    }
                }
            }
            Command::Scrape => {
                for id in self.in_use(&g, now) {
                    let floor = g.trackers[id].reannounce_floor(&self.timings);
                    let t = &mut g.trackers[id];
                    if t.scrapable
                        && !t.scrape_in_flight
                        && t.last_scrape.is_none_or(|l| now.duration_since(l) >= floor)
                    {
                        t.scrape_wanted = true;
                    }
                }
            }
        }
    }

    fn next_interval(&self, outcome: &Outcome) -> Duration {
        let interval = match (outcome.interval, outcome.min_interval) {
            (Some(i), Some(m)) => i.max(m),
            (Some(i), None) => i,
            (None, Some(m)) => m,
            (None, None) => DEFAULT_INTERVAL,
        }
        .max(MIN_INTERVAL);
        self.force_interval.unwrap_or(interval).min(MAX_INTERVAL)
    }

    fn retry_delay(&self, fails: u32) -> Duration {
        let exp = fails.saturating_sub(1).min(20);
        let base = self
            .timings
            .retry_min
            .saturating_mul(1u32 << exp)
            .min(self.timings.retry_max);
        base + base.mul_f64(rand::random::<f64>() * 0.5)
    }

    /// Records the result of an announce, and returns the peers it brought.
    fn apply(
        &self,
        generation: u64,
        id: usize,
        event: Option<TrackerRequestEvent>,
        result: JobResult,
    ) -> Vec<SocketAddr> {
        let mut g = self.inner.lock();
        if g.driver.as_ref().map(|d| d.generation) != Some(generation) {
            return Vec::new();
        }
        let now = Instant::now();
        let t = &mut g.trackers[id];
        t.in_flight = false;
        if result.resolved.is_some() {
            t.resolved = result.resolved;
        }
        let mut promote = false;
        let mut addrs = Vec::new();
        match result.result {
            Ok(outcome) => {
                let interval = self.next_interval(&outcome);
                promote = t.state != TrackerState::Working;
                t.state = TrackerState::Working;
                t.fails = 0;
                t.last_error = None;
                t.last_announce = Some(SystemTime::now());
                t.peers_received = Some(u32::try_from(outcome.addrs.len()).unwrap_or(u32::MAX));
                t.seeders = outcome.seeders.or(t.seeders);
                t.leechers = outcome.leechers.or(t.leechers);
                t.downloaded = outcome.downloaded.or(t.downloaded);
                t.warning = outcome.warning;
                if outcome.tracker_id.is_some() {
                    t.tracker_id = outcome.tracker_id;
                }
                t.min_interval = outcome.min_interval;
                match event {
                    Some(TrackerRequestEvent::Started) => {
                        t.needs_started = false;
                        t.started_acked = true;
                        // A `started` that said nothing is left makes `completed` pointless;
                        // one sent before the download finished does not.
                        if outcome.left == 0 {
                            t.pending_completed = false;
                        }
                    }
                    Some(TrackerRequestEvent::Completed) => t.pending_completed = false,
                    _ => {}
                }
                // A `completed` that was asked for while this request was in flight goes
                // right away.
                t.next_at = Some(if t.pending_completed {
                    now
                } else {
                    now + interval
                });
                // A tracker that can be scraped is, once per announce interval, unless the
                // announce gave all the numbers already.
                if t.auto_scrape
                    && outcome.downloaded.is_none()
                    && t.last_scrape
                        .is_none_or(|l| now.duration_since(l) >= interval)
                {
                    t.scrape_wanted = true;
                }
                addrs = outcome.addrs;
            }
            Err(e) => {
                t.state = TrackerState::Error;
                t.fails = t.fails.saturating_add(1);
                t.last_error = Some(bounded_text(format!("{e:#}")));
                t.warning = None;
                t.next_at = Some(now + self.retry_delay(t.fails));
                if result.panicked {
                    t.disabled = true;
                }
            }
        }
        if promote
            && let Some(tier) = g.tiers.iter_mut().find(|tier| tier.contains(&id))
            && let Some(pos) = tier.iter().position(|&i| i == id)
        {
            tier[..=pos].rotate_right(1);
        }
        addrs
    }

    fn apply_scrape(&self, generation: u64, id: usize, result: ScrapeResult) {
        let mut g = self.inner.lock();
        if g.driver.as_ref().map(|d| d.generation) != Some(generation) {
            return;
        }
        let t = &mut g.trackers[id];
        t.scrape_in_flight = false;
        if result.resolved.is_some() {
            t.resolved = result.resolved;
        }
        // A scrape that fails says nothing about whether announces work, so only the
        // numbers are touched.
        match result.result {
            Ok(c) => {
                t.seeders = c.seeders.or(t.seeders);
                t.leechers = c.leechers.or(t.leechers);
                t.downloaded = c.downloaded.or(t.downloaded);
            }
            Err(e) => {
                // Most likely the tracker does not do scrapes: stop asking by ourselves.
                t.auto_scrape = false;
                debug!(tracker = %t.url, "scrape failed: {e:#}");
            }
        }
    }

    /// One request to one tracker, isolated from the others: a panic inside of it (e.g. in
    /// a TLS verifier) disables the tracker instead of taking the torrent's peer discovery
    /// down.
    fn job_future(self: &Arc<Self>, job: Job, timeout: Duration) -> JobFuture {
        let this = self.clone();
        let span = match job.kind {
            Kind::Udp => {
                debug_span!(parent: None, "udp_tracker", tracker = %job.url, info_hash = ?self.info_hash)
            }
            _ => {
                debug_span!(parent: None, "http_tracker", tracker = %job.url, info_hash = ?self.info_hash)
            }
        };
        async move {
            let id = job.id;
            let event = job.event;
            let url = job.url.clone();
            let work = async {
                match job.kind {
                    Kind::Http => JobResult {
                        result: this.announce_http(&job).await,
                        resolved: None,
                        panicked: false,
                    },
                    Kind::Udp => this.announce_udp(&job).await,
                    Kind::Unsupported => JobResult::failed(anyhow::anyhow!("unsupported tracker")),
                }
            };
            let res = std::panic::AssertUnwindSafe(tokio::time::timeout(timeout, work))
                .catch_unwind()
                .await;
            let jr = match res {
                Ok(Ok(jr)) => jr,
                Ok(Err(_)) => JobResult::failed(anyhow::anyhow!("timed out after {timeout:?}")),
                Err(panic) => {
                    let msg = panic
                        .downcast_ref::<&str>()
                        .copied()
                        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                        .unwrap_or("<non-string panic payload>");
                    tracing::error!(tracker = %url, "tracker monitor panicked, disabling this tracker: {msg}");
                    JobResult {
                        result: Err(anyhow::anyhow!("panicked: {msg}")),
                        resolved: None,
                        panicked: true,
                    }
                }
            };
            match &jr.result {
                Ok(outcome) => {
                    debug!(
                        "sleeping for {:?} after calling tracker",
                        this.next_interval(outcome)
                    )
                }
                Err(err) => debug!("error calling tracker: {err:#}"),
            }
            (id, event, jr)
        }
        .instrument(span)
        .boxed()
    }

    fn stats_for(&self, job: &Job) -> TrackerCommsStats {
        job.stats.unwrap_or_else(|| self.stats.get())
    }

    async fn announce_http(&self, job: &Job) -> anyhow::Result<Outcome> {
        let stats = self.stats_for(job);
        let left = stats.get_left_to_download_bytes();
        let request = tracker_comms_http::TrackerRequest {
            info_hash: &self.info_hash,
            peer_id: &self.peer_id,
            port: self.announce_port,
            uploaded: stats.uploaded_bytes,
            downloaded: stats.downloaded_bytes,
            left,
            compact: true,
            no_peer_id: false,
            event: job.event,
            ip: None,
            numwant: job.no_peers.then_some(0),
            key: Some(self.key),
            trackerid: job.tracker_id.as_deref(),
        };

        let mut url = job.url.clone();
        let mut queries = request.as_querystring();
        if let Some(url_query) = url.query() {
            queries.push_str(&format!("&{}", url_query));
        }
        url.set_query(Some(&queries));

        // `without_url`: the URL carries the passkey, the key and the tracker id, and the
        // error ends up in `TrackerStats::last_error`.
        let response: reqwest::Response = self
            .reqwest_client
            .get(url)
            .send()
            .await
            .map_err(|e| e.without_url())?;
        if !response.status().is_success() {
            anyhow::bail!("tracker responded with {:?}", response.status());
        }
        let bytes = read_body_limited(response).await?;
        let mut outcome = parse_http_announce(&bytes)?;
        outcome.left = left;
        Ok(outcome)
    }

    async fn announce_udp(&self, job: &Job) -> JobResult {
        let (host, port) = match (job.url.host(), job.url.port()) {
            (Some(h), Some(p)) => (h, p),
            _ => return JobResult::failed(anyhow::anyhow!("tracker URL needs a host and a port")),
        };
        // A proxy that can't relay UDP: fail before anything (DNS included) is done.
        if let Err(e) = self.udp_client.check_usable() {
            return JobResult::failed(e.into());
        }
        // A `socks5h` proxy resolves the host name: it is never looked up here.
        if let url::Host::Domain(name) = host
            && self.udp_client.sends_host_names()
        {
            let stats = self.stats_for(job);
            return JobResult {
                result: self
                    .announce_udp_endpoint(TargetAddr::Domain(name, port), job, &stats)
                    .await,
                resolved: None,
                panicked: false,
            };
        }
        let resolved = match udp_tracker_to_socket_addrs(host, port)
            .instrument(trace_span!("resolve", host=%job.url))
            .await
        {
            Ok(r) => r,
            Err(e) => match job.resolved {
                Some(r) => {
                    debug!("error resolving tracker, using the old addresses: {e:#}");
                    r
                }
                None => return JobResult::failed(e.context("error resolving tracker")),
            },
        };
        let stats = self.stats_for(job);
        let result = match resolved {
            UdpTrackerResolveResult::One(addr) => {
                self.announce_udp_endpoint(addr.into(), job, &stats).await
            }
            UdpTrackerResolveResult::Two(v4, v6) => {
                let (r4, r6) = tokio::join!(
                    self.announce_udp_endpoint(SocketAddr::from(v4).into(), job, &stats)
                        .instrument(trace_span!("udp request", addr=?v4)),
                    self.announce_udp_endpoint(SocketAddr::from(v6).into(), job, &stats)
                        .instrument(trace_span!("udp request", addr=?v6)),
                );
                merge_endpoints(r4, r6)
            }
        };
        JobResult {
            result,
            resolved: Some(resolved),
            panicked: false,
        }
    }

    async fn announce_udp_endpoint(
        &self,
        addr: TargetAddr<'_>,
        job: &Job,
        stats: &TrackerCommsStats,
    ) -> anyhow::Result<Outcome> {
        use tracker_comms_udp::*;
        let left = stats.get_left_to_download_bytes();
        let request = AnnounceFields {
            info_hash: self.info_hash,
            peer_id: self.peer_id,
            downloaded: stats.downloaded_bytes,
            left,
            uploaded: stats.uploaded_bytes,
            event: match job.event {
                None => EVENT_NONE,
                Some(TrackerRequestEvent::Started) => EVENT_STARTED,
                Some(TrackerRequestEvent::Completed) => EVENT_COMPLETED,
                Some(TrackerRequestEvent::Stopped) => EVENT_STOPPED,
            },
            key: self.key,
            port: self.announce_port,
            num_want: if job.no_peers { 0 } else { -1 },
        };
        match self.udp_client.announce_to(addr, request).await {
            Ok(response) => {
                trace!(len = response.addrs.len(), "received announce response");
                Ok(Outcome {
                    interval: Some(tracker_interval(u64::from(response.interval))),
                    seeders: Some(response.seeders),
                    leechers: Some(response.leechers),
                    addrs: response.addrs,
                    left,
                    ..Default::default()
                })
            }
            Err(e) => {
                debug!(?addr, "error reading announce response: {e:#}");
                Err(e)
            }
        }
    }

    /// One scrape of one tracker, isolated like the announces are.
    fn scrape_future(self: &Arc<Self>, job: ScrapeJob, timeout: Duration) -> ScrapeFuture {
        let this = self.clone();
        let span = match job.kind {
            Kind::Udp => {
                debug_span!(parent: None, "udp_tracker", tracker = %job.url, info_hash = ?self.info_hash)
            }
            _ => {
                debug_span!(parent: None, "http_tracker", tracker = %job.url, info_hash = ?self.info_hash)
            }
        };
        async move {
            let id = job.id;
            let work = async {
                match job.kind {
                    Kind::Http => ScrapeResult {
                        result: this.scrape_http(&job).await,
                        resolved: None,
                    },
                    Kind::Udp => this.scrape_udp(&job).await,
                    Kind::Unsupported => ScrapeResult {
                        result: Err(anyhow::anyhow!("unsupported tracker")),
                        resolved: None,
                    },
                }
            };
            let res = std::panic::AssertUnwindSafe(tokio::time::timeout(timeout, work))
                .catch_unwind()
                .await;
            let result = match res {
                Ok(Ok(r)) => r,
                Ok(Err(_)) => ScrapeResult {
                    result: Err(anyhow::anyhow!("timed out after {timeout:?}")),
                    resolved: None,
                },
                Err(_) => ScrapeResult {
                    result: Err(anyhow::anyhow!("the scrape panicked")),
                    resolved: None,
                },
            };
            (id, result)
        }
        .instrument(span)
        .boxed()
    }

    async fn scrape_http(&self, job: &ScrapeJob) -> anyhow::Result<ScrapeCounts> {
        let url = scrape::scrape_url(&job.url, &self.info_hash)
            .context("the tracker can't be scraped")?;
        let response = self
            .reqwest_client
            .get(url)
            .send()
            .await
            .map_err(|e| e.without_url())?;
        if !response.status().is_success() {
            anyhow::bail!("tracker responded with {:?}", response.status());
        }
        let bytes = read_body_limited(response).await?;
        scrape::parse_scrape(&bytes, &self.info_hash)
    }

    async fn scrape_udp(&self, job: &ScrapeJob) -> ScrapeResult {
        let (host, port) = match (job.url.host(), job.url.port()) {
            (Some(h), Some(p)) => (h, p),
            _ => {
                return ScrapeResult {
                    result: Err(anyhow::anyhow!("tracker URL needs a host and a port")),
                    resolved: None,
                };
            }
        };
        if let Err(e) = self.udp_client.check_usable() {
            return ScrapeResult {
                result: Err(e.into()),
                resolved: None,
            };
        }
        let one = |target| self.scrape_udp_target(target);
        // A `socks5h` proxy resolves the host name: it is never looked up here.
        if let url::Host::Domain(name) = host
            && self.udp_client.sends_host_names()
        {
            return ScrapeResult {
                result: one(TargetAddr::Domain(name, port)).await,
                resolved: None,
            };
        }
        let resolved = match udp_tracker_to_socket_addrs(host, port)
            .instrument(trace_span!("resolve", host=%job.url))
            .await
        {
            Ok(r) => r,
            Err(e) => match job.resolved {
                Some(r) => r,
                None => {
                    return ScrapeResult {
                        result: Err(e.context("error resolving tracker")),
                        resolved: None,
                    };
                }
            },
        };
        // Like the announces: both addresses of a dual-stack tracker are asked, and the larger
        // numbers win.
        let result = match resolved {
            UdpTrackerResolveResult::One(addr) => one(addr.into()).await,
            UdpTrackerResolveResult::Two(v4, v6) => {
                let (r4, r6) = tokio::join!(
                    one(SocketAddr::from(v4).into()),
                    one(SocketAddr::from(v6).into())
                );
                match (r4, r6) {
                    (Ok(a), Ok(b)) => Ok(a.max(b)),
                    (Ok(a), Err(_)) => Ok(a),
                    (Err(_), Ok(b)) => Ok(b),
                    (Err(a), Err(b)) => Err(anyhow::anyhow!("IPv4: {a:#}; IPv6: {b:#}")),
                }
            }
        };
        ScrapeResult {
            result,
            resolved: Some(resolved),
        }
    }

    /// Scrapes one address (or host name, resolved by the proxy) of a UDP tracker.
    async fn scrape_udp_target(&self, target: TargetAddr<'_>) -> anyhow::Result<ScrapeCounts> {
        self.udp_client
            .scrape_from(target, self.info_hash)
            .await
            .map(|r| ScrapeCounts {
                seeders: Some(r.seeders),
                leechers: Some(r.leechers),
                downloaded: Some(r.completed),
            })
    }
}

/// Parses an announce response of an HTTP tracker. `left` is left at 0 for the caller.
fn parse_http_announce(bytes: &[u8]) -> anyhow::Result<Outcome> {
    if let Ok((error, _)) = bencode::from_bytes_with_rest::<tracker_comms_http::TrackerError>(bytes)
    {
        anyhow::bail!(
            "tracker returned failure. Failure reason: {}",
            bounded_text(error.failure_reason.to_string())
        )
    };
    let response = bencode::from_bytes_with_rest::<tracker_comms_http::TrackerResponse>(bytes)
        .map_err(|e| {
            tracing::trace!("error deserializing TrackerResponse: {e:#}");
            e.into_kind()
        })?
        .0;
    Ok(Outcome {
        interval: response.interval.map(tracker_interval),
        min_interval: response.min_interval.map(tracker_interval),
        seeders: response.complete.map(count),
        leechers: response.incomplete.map(count),
        downloaded: response.downloaded.map(count),
        addrs: response.iter_peers().collect(),
        left: 0,
        warning: response
            .warning_message
            .map(|w| bounded_text(String::from_utf8_lossy(w.as_ref()).into_owned())),
        // An id too long to be a real one is not worth sending back with every announce.
        tracker_id: response
            .tracker_id
            .filter(|id| id.as_ref().len() <= MAX_TRACKER_ID_LEN)
            .map(|id| id.as_ref().to_vec()),
    })
}

/// A tracker that resolves to an IPv4 and an IPv6 address is announced to on both. It works
/// when either does, and it fails only when both fail.
fn merge_endpoints(
    a: anyhow::Result<Outcome>,
    b: anyhow::Result<Outcome>,
) -> anyhow::Result<Outcome> {
    match (a, b) {
        (Ok(mut a), Ok(b)) => {
            a.addrs.extend(b.addrs);
            Ok(Outcome {
                interval: match (a.interval, b.interval) {
                    (Some(x), Some(y)) => Some(x.min(y)),
                    (x, y) => x.or(y),
                },
                min_interval: a.min_interval.max(b.min_interval),
                seeders: a.seeders.max(b.seeders),
                leechers: a.leechers.max(b.leechers),
                downloaded: a.downloaded.max(b.downloaded),
                addrs: a.addrs,
                left: a.left,
                warning: a.warning.or(b.warning),
                tracker_id: a.tracker_id.or(b.tracker_id),
            })
        }
        (Ok(a), Err(_)) => Ok(a),
        (Err(_), Ok(b)) => Ok(b),
        (Err(a), Err(b)) => Err(anyhow::anyhow!("IPv4: {a:#}; IPv6: {b:#}")),
    }
}
