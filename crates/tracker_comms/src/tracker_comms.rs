use std::collections::HashSet;
use std::net::SocketAddr;
use std::net::SocketAddrV4;
use std::net::SocketAddrV6;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::bail;
use backon::ExponentialBuilder;
use backon::Retryable;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::Either;
use futures::stream::BoxStream;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing::debug;
use tracing::debug_span;
use tracing::trace;
use tracing::trace_span;
use url::Url;

use crate::tracker_comms_http;
use crate::tracker_comms_udp;
use crate::tracker_comms_udp::UdpTrackerClient;
use librqbit_core::hash_id::Id20;

pub struct TrackerComms {
    info_hash: Id20,
    peer_id: Id20,
    stats: Box<dyn TorrentStatsProvider>,
    force_tracker_interval: Option<Duration>,
    tx: Sender,
    // This MUST be set as trackers don't work with 0 port.
    announce_port: u16,
    reqwest_client: reqwest::Client,
    key: u32,
    tracker_immediate_tx: Mutex<Vec<tokio::sync::mpsc::Sender<TrackerImmediateEvent>>>,
}

#[derive(Default)]
pub enum TrackerCommsStatsState {
    #[default]
    None,
    Initializing,
    Paused,
    Live,
}

#[derive(Default)]
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

type Sender = tokio::sync::mpsc::Sender<SocketAddr>;

enum SupportedTracker {
    Udp(Url),
    Http(Url),
}

impl std::fmt::Debug for SupportedTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SupportedTracker::Udp(u) => std::fmt::Display::fmt(u, f),
            SupportedTracker::Http(u) => std::fmt::Display::fmt(u, f),
        }
    }
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

impl TrackerComms {
    // TODO: fix too many args
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        info_hash: Id20,
        peer_id: Id20,
        trackers: HashSet<Url>,
        stats: Box<dyn TorrentStatsProvider>,
        force_interval: Option<Duration>,
        announce_port: u16,
        reqwest_client: reqwest::Client,
        udp_client: UdpTrackerClient,
    ) -> Option<(TrackerHandle, BoxStream<'static, SocketAddr>)> {
        let trackers = trackers
            .into_iter()
            .filter_map(|t| match t.scheme() {
                "http" | "https" => Some(SupportedTracker::Http(t)),
                "udp" => Some(SupportedTracker::Udp(t)),
                _ => {
                    debug!("unsupported tracker URL: {}", t);
                    None
                }
            })
            .collect::<Vec<_>>();
        if trackers.is_empty() {
            debug!(?info_hash, "trackers list is empty");
            return None;
        }

        tracing::trace!(?trackers);

        let (tx, mut rx) = tokio::sync::mpsc::channel::<SocketAddr>(16);

        let comms = Arc::new(Self {
            info_hash,
            peer_id,
            stats,
            force_tracker_interval: force_interval,
            tx,
            announce_port,
            reqwest_client,
            key: rand::random(),
            tracker_immediate_tx: Mutex::new(vec![]),
        });

        let cancel = CancellationToken::new();

        let handle = TrackerHandle {
            cancel: cancel.clone(),
            comms: comms.clone(),
        };

        for tracker in trackers {
            let cancel = cancel.clone();
            let comms = comms.clone();
            let udp = udp_client.clone();

            tokio::spawn(async move {
                comms.run_tracker(tracker, &udp, cancel).await;
            });
        }

        let s = async_stream::stream! {
            while let Some(addr) = rx.recv().await {
                yield addr;
            }
        };

        Some((handle, s.boxed()))
    }

    fn add_tracker(
        &self,
        url: SupportedTracker,
        client: &UdpTrackerClient,
        cancel: CancellationToken,
    ) -> Either<
        impl std::future::Future<Output = anyhow::Result<()>> + '_ + Send,
        impl std::future::Future<Output = anyhow::Result<()>> + '_ + Send,
    > {
        let info_hash = self.info_hash;
        let (immediate_tx, immediate_rx) = tokio::sync::mpsc::channel::<TrackerImmediateEvent>(8);
        self.tracker_immediate_tx.lock().push(immediate_tx);
        match url {
            SupportedTracker::Udp(url) => {
                let span = debug_span!(parent: None, "udp_tracker", tracker = %url, info_hash = ?info_hash);
                self.task_single_tracker_monitor_udp(url, client.clone())
                    .instrument(span)
                    .right_future()
            }
            SupportedTracker::Http(url) => {
                let span = debug_span!(
                    parent: None,
                    "http_tracker",
                    tracker = %url,
                    info_hash = ?info_hash
                );
                self.task_single_tracker_monitor_http(url, immediate_rx, cancel)
                    .instrument(span)
                    .left_future()
            }
        }
    }

    async fn task_single_tracker_monitor_http(
        &self,
        tracker_url: Url,
        mut immediate_rx: tokio::sync::mpsc::Receiver<TrackerImmediateEvent>,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        trace!(url=%tracker_url, "starting monitor");
        let mut started = false;
        let mut interval = Duration::from_secs(0);

        loop {
            tokio::select! {
                Some(ev) = immediate_rx.recv() => {
                    match ev {
                        TrackerImmediateEvent::Started => {
                            if started {
                                continue;
                            }

                            let next = (|| {
                                self.tracker_one_request_http(
                                    &tracker_url,
                                    Some(tracker_comms_http::TrackerRequestEvent::Started),
                                )
                            })
                            .retry(
                                ExponentialBuilder::new()
                                    .without_max_times()
                                    .with_jitter()
                                    .with_min_delay(Duration::from_secs(10))
                                    .with_max_delay(Duration::from_secs(600)),
                            )
                            .notify(|err, retry_in| {
                                debug!(?retry_in, "error sending started event: {err:#}")
                            })
                            .await
                            .expect("started retry is infinite");

                            started = true;
                            interval = self.force_tracker_interval.unwrap_or(next);
                            interval = interval.max(Duration::from_secs(15));
                        }

                        TrackerImmediateEvent::Completed => {
                            let _ = self.tracker_one_request_http(
                                &tracker_url,
                                Some(tracker_comms_http::TrackerRequestEvent::Completed),
                            ).await;
                        }

                        TrackerImmediateEvent::Stopped(ack) => {
                            let _ = self.tracker_one_request_http(
                                &tracker_url,
                                Some(tracker_comms_http::TrackerRequestEvent::Stopped),
                            ).await;
                            let _ = ack.send(());
                            break;
                        }
                    }
                }

                _ = tokio::time::sleep(interval) => {
                    if !started {
                        continue;
                    }

                    interval = (|| self.tracker_one_request_http(&tracker_url, None))
                        .retry(
                            ExponentialBuilder::new()
                                .without_max_times()
                                .with_jitter()
                                .with_factor(2.)
                                .with_min_delay(Duration::from_secs(10))
                                .with_max_delay(Duration::from_secs(600)),
                        )
                        .notify(|err, retry_in| debug!(?retry_in, "error calling tracker: {err:#}"))
                        .await
                        .context("this shouldnt fail")?;

                    interval = self.force_tracker_interval.unwrap_or(interval);
                    // Enforce a minimum interval of 15 seconds to avoid hammering trackers.
                    interval = interval.max(Duration::from_secs(15));
                    debug!("sleeping for {:?} after calling tracker", interval);
                }

                _ = cancel.cancelled() => {
                    break;
                }
            }
        }

        Ok(())
    }

    async fn tracker_one_request_http(
        &self,
        tracker_url: &Url,
        event: Option<tracker_comms_http::TrackerRequestEvent>,
    ) -> anyhow::Result<Duration> {
        let stats = self.stats.get();
        let request = tracker_comms_http::TrackerRequest {
            info_hash: &self.info_hash,
            peer_id: &self.peer_id,
            port: self.announce_port,
            uploaded: stats.uploaded_bytes,
            downloaded: stats.downloaded_bytes,
            left: stats.get_left_to_download_bytes(),
            compact: true,
            no_peer_id: false,
            event,
            ip: None,
            numwant: None,
            key: Some(self.key),
            trackerid: None,
        };

        let mut url = tracker_url.clone();
        url.set_query(Some(&request.as_querystring()));

        let response: reqwest::Response = self.reqwest_client.get(url).send().await?;
        if !response.status().is_success() {
            anyhow::bail!("tracker responded with {:?}", response.status());
        }
        let bytes = response.bytes().await?;
        if let Ok((error, _)) =
            bencode::from_bytes_with_rest::<tracker_comms_http::TrackerError>(&bytes)
        {
            anyhow::bail!(
                "tracker returned failure. Failure reason: {}",
                error.failure_reason
            )
        };
        let response = bencode::from_bytes_with_rest::<tracker_comms_http::TrackerResponse>(&bytes)
            .map_err(|e| {
                tracing::trace!("error deserializing TrackerResponse: {e:#}");
                e.into_kind()
            })?
            .0;

        for peer in response.iter_peers() {
            self.tx.send(peer).await?;
        }
        Ok(Duration::from_secs(response.interval))
    }

    async fn task_single_tracker_monitor_udp(
        &self,
        url: Url,
        client: UdpTrackerClient,
    ) -> anyhow::Result<()> {
        if url.scheme() != "udp" {
            bail!("expected UDP scheme in {}", url);
        }
        let (host, port) = (
            url.host().context("missing host")?,
            url.port().context("missing port")?,
        );

        let mut sleep_interval: Option<Duration> = None;
        let mut prev_addrs: Option<UdpTrackerResolveResult> = None;
        loop {
            if let Some(i) = sleep_interval {
                trace!(interval=?sleep_interval, "sleeping");
                tokio::time::sleep(i).await;
            }

            // This should retry forever until the addrs are resolved.
            let addrs = (async || {
                udp_tracker_to_socket_addrs(host.clone(), port)
                    .instrument(trace_span!("resolve", ?host))
                    .await
                    .or_else(|err| prev_addrs.ok_or(err))
            })
            .retry(
                ExponentialBuilder::new()
                    .without_max_times()
                    .with_max_delay(Duration::from_secs(60))
                    .with_jitter(),
            )
            .notify(|err, retry| debug!(retry_in=?retry, "error resolving tracker: {err:#}"))
            .await
            .context("this shouldn't happen: failed resolving tracker addrs")?;

            prev_addrs = Some(addrs);

            match addrs {
                UdpTrackerResolveResult::One(addr) => {
                    match self
                        .tracker_one_request_udp(addr, &client)
                        .instrument(trace_span!("udp request", ?addr))
                        .await
                    {
                        Ok(sleep) => sleep_interval = Some(sleep),
                        Err(_) => {
                            sleep_interval = Some(sleep_interval.unwrap_or(Duration::from_secs(60)))
                        }
                    }
                }
                UdpTrackerResolveResult::Two(v4, v6) => {
                    let (r4, r6) = tokio::join!(
                        self.tracker_one_request_udp(v4.into(), &client)
                            .instrument(trace_span!("udp request", addr=?v4)),
                        self.tracker_one_request_udp(v6.into(), &client)
                            .instrument(trace_span!("udp request", addr=?v6))
                    );
                    sleep_interval = Some(
                        r4.or(r6)
                            .ok()
                            .or(sleep_interval)
                            .unwrap_or(Duration::from_secs(60)),
                    )
                }
            }
        }
    }

    async fn tracker_one_request_udp(
        &self,
        addr: SocketAddr,
        client: &UdpTrackerClient,
    ) -> anyhow::Result<Duration> {
        use tracker_comms_udp::*;

        let stats = self.stats.get();
        let request = AnnounceFields {
            info_hash: self.info_hash,
            peer_id: self.peer_id,
            downloaded: stats.downloaded_bytes,
            left: stats.get_left_to_download_bytes(),
            uploaded: stats.uploaded_bytes,
            event: match stats.torrent_state {
                TrackerCommsStatsState::None => EVENT_NONE,
                TrackerCommsStatsState::Initializing => EVENT_STARTED,
                TrackerCommsStatsState::Paused => EVENT_STOPPED,
                TrackerCommsStatsState::Live => {
                    if stats.is_completed() {
                        EVENT_COMPLETED
                    } else {
                        EVENT_STARTED
                    }
                }
            },
            key: self.key,
            port: self.announce_port,
        };

        match client.announce(addr, request).await {
            Ok(response) => {
                trace!(len = response.addrs.len(), "received announce response");
                for addr in response.addrs {
                    self.tx.send(addr).await.context("rx closed")?;
                }
                let sleep = response.interval.max(5);
                let sleep = Duration::from_secs(sleep as u64);
                Ok(sleep)
            }
            Err(e) => {
                debug!(?addr, "error reading announce response: {e:#}");
                Err(e)
            }
        }
    }

    async fn run_tracker(
        &self,
        tracker: SupportedTracker,
        udp: &UdpTrackerClient,
        cancel: CancellationToken,
    ) {
        let (immediate_tx, immediate_rx) = tokio::sync::mpsc::channel::<TrackerImmediateEvent>(8);

        self.tracker_immediate_tx.lock().push(immediate_tx);

        match tracker {
            SupportedTracker::Http(url) => {
                let _ = self
                    .task_single_tracker_monitor_http(url, immediate_rx, cancel)
                    .await;
            }
            SupportedTracker::Udp(url) => {
                let _ = self.task_single_tracker_monitor_udp(url, udp.clone()).await;
            }
        }
    }

    async fn notify_started(&self) {
        let txs = self.tracker_immediate_tx.lock().clone();

        for tx in txs.iter() {
            let _ = tx.send(TrackerImmediateEvent::Started).await;
        }
    }

    async fn notify_completed(&self) {
        let txs = self.tracker_immediate_tx.lock().clone();

        for tx in txs.iter() {
            let _ = tx.send(TrackerImmediateEvent::Completed).await;
        }
    }

    async fn notify_stopped(&self) {
        let txs = self.tracker_immediate_tx.lock().clone();

        for tx in txs.iter() {
            let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();

            let _ = tx.send(TrackerImmediateEvent::Stopped(ack_tx)).await;

            let _ = ack_rx.await;
        }
    }
}

#[derive(Clone)]
pub struct TrackerHandle {
    cancel: CancellationToken,
    comms: Arc<TrackerComms>,
}

impl TrackerHandle {
    pub async fn notify_started(&self) {
        self.comms.notify_started().await;
    }

    pub async fn notify_completed(&self) {
        self.comms.notify_completed().await;
    }

    pub async fn notify_stopped(&self) {
        self.comms.notify_stopped().await;
        self.cancel.cancel();
    }
}

pub enum TrackerImmediateEvent {
    Started,
    Completed,
    Stopped(tokio::sync::oneshot::Sender<()>),
}
