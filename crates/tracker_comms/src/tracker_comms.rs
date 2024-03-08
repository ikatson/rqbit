use std::net::SocketAddr;
use std::time::Duration;

use anyhow::bail;
use anyhow::Context;
use futures::future::BoxFuture;
use futures::future::Either;
use futures::stream::BoxStream;
use futures::stream::FuturesUnordered;
use futures::FutureExt;
use futures::StreamExt;
use tracing::debug;
use tracing::error_span;
use tracing::trace;
use tracing::Instrument;
use url::Url;

use crate::tracker_comms_http;
use crate::tracker_comms_udp;
use librqbit_core::hash_id::Id20;

pub struct TrackerCommsStandard {}

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

pub trait TorrentActionsProvider: Send + Sync {
    fn forget_all_peers(&self) -> BoxFuture<'_, anyhow::Result<()>>;
}

impl TorrentStatsProvider for () {
    fn get(&self) -> TrackerCommsStats {
        Default::default()
    }
}

impl TorrentActionsProvider for () {
    fn forget_all_peers(&self) -> BoxFuture<'_, anyhow::Result<()>> {
        async { Ok(()) }.boxed()
    }
}

enum SupportedTracker {
    Udp(Url),
    Http(Url),
}

pub struct ManagedTorrentInfoForTrackerMonitor {
    pub info_hash: Id20,
    pub peer_id: Id20,
    pub stats: Box<dyn TorrentStatsProvider>,
    pub actions: Box<dyn TorrentActionsProvider>,
    pub force_interval: Option<Duration>,
    pub tcp_listen_port: Option<u16>,
}

async fn task_single_tracker_monitor_http(
    info: &ManagedTorrentInfoForTrackerMonitor,
    tx: &Tx,
    mut tracker_url: Url,
) -> anyhow::Result<()> {
    let mut event = Some(tracker_comms_http::TrackerRequestEvent::Started);
    loop {
        let stats = info.stats.get();
        let request = tracker_comms_http::TrackerRequest {
            info_hash: info.info_hash,
            peer_id: info.peer_id,
            port: info.tcp_listen_port.unwrap_or(0),
            uploaded: stats.uploaded_bytes,
            downloaded: stats.downloaded_bytes,
            left: stats.get_left_to_download_bytes(),
            compact: true,
            no_peer_id: false,
            event,
            ip: None,
            numwant: None,
            key: None,
            trackerid: None,
        };

        let request_query = request.as_querystring();
        tracker_url.set_query(Some(&request_query));

        match tracker_one_request_http(tx, tracker_url.clone()).await {
            Ok(interval) => {
                event = None;
                let interval = info
                    .force_interval
                    .unwrap_or_else(|| Duration::from_secs(interval));
                debug!(
                    "sleeping for {:?} after calling tracker {}",
                    interval,
                    tracker_url.host().unwrap()
                );
                tokio::time::sleep(interval).await;
            }
            Err(e) => {
                debug!("error calling the tracker {}: {:#}", tracker_url, e);
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        };
    }
}

async fn tracker_one_request_http(tx: &Tx, tracker_url: Url) -> anyhow::Result<u64> {
    let response: reqwest::Response = reqwest::get(tracker_url).await?;
    if !response.status().is_success() {
        anyhow::bail!("tracker responded with {:?}", response.status());
    }
    let bytes = response.bytes().await?;
    if let Ok(error) = bencode::from_bytes::<tracker_comms_http::TrackerError>(&bytes) {
        anyhow::bail!(
            "tracker returned failure. Failure reason: {}",
            error.failure_reason
        )
    };
    let response = bencode::from_bytes::<tracker_comms_http::TrackerResponse>(&bytes)?;

    for peer in response.peers.iter_sockaddrs() {
        tx.send(peer).await?;
    }
    Ok(response.interval)
}

type Tx = tokio::sync::mpsc::Sender<SocketAddr>;

async fn task_single_tracker_monitor_udp(
    info: &ManagedTorrentInfoForTrackerMonitor,
    tx: &Tx,
    url: Url,
) -> anyhow::Result<()> {
    use tracker_comms_udp::*;

    if url.scheme() != "udp" {
        bail!("expected UDP scheme in {}", url);
    }
    let hp: (&str, u16) = (
        url.host_str().context("missing host")?,
        url.port().context("missing port")?,
    );
    let mut requester = UdpTrackerRequester::new(hp)
        .await
        .context("error creating UDP tracker requester")?;

    let mut sleep_interval: Option<Duration> = None;
    loop {
        if let Some(i) = sleep_interval {
            trace!(interval=?sleep_interval, "sleeping");
            tokio::time::sleep(i).await;
        }

        let stats = info.stats.get();
        let request = AnnounceFields {
            info_hash: info.info_hash,
            peer_id: info.peer_id,
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
            key: 0, // whatever that is?
            port: info.tcp_listen_port.unwrap_or(0),
        };

        match requester.announce(request).await {
            Ok(response) => {
                trace!(len = response.addrs.len(), "received announce response");
                for addr in response.addrs {
                    tx.send(SocketAddr::V4(addr)).await.context("rx closed")?;
                }
                let new_interval = response.interval.max(5);
                let new_interval = Duration::from_secs(new_interval as u64);
                sleep_interval = Some(info.force_interval.unwrap_or(new_interval));
            }
            Err(e) => {
                debug!(url = ?url, "error reading announce response: {e:#}");
                if sleep_interval.is_none() {
                    sleep_interval = Some(info.force_interval.unwrap_or(Duration::from_secs(60)));
                }
            }
        }
    }
}

fn task_single_tracker_monitor<'a>(
    info: &'a ManagedTorrentInfoForTrackerMonitor,
    tx: &'a Tx,
    url: SupportedTracker,
) -> Either<
    impl std::future::Future<Output = anyhow::Result<()>> + 'a + Send,
    impl std::future::Future<Output = anyhow::Result<()>> + 'a + Send,
> {
    let info_hash = info.info_hash;
    match url {
        SupportedTracker::Udp(url) => {
            let span =
                error_span!(parent: None, "udp_tracker", tracker = %url, info_hash = ?info_hash);
            task_single_tracker_monitor_udp(info, tx, url)
                .instrument(span)
                .right_future()
        }
        SupportedTracker::Http(url) => {
            let span = error_span!(
                parent: None,
                "http_tracker",
                tracker = %url,
                info_hash = ?info_hash
            );
            task_single_tracker_monitor_http(info, tx, url)
                .instrument(span)
                .left_future()
        }
    }
}

impl TrackerCommsStandard {
    pub fn start(
        info: ManagedTorrentInfoForTrackerMonitor,
        trackers: Vec<String>,
    ) -> Option<BoxStream<'static, SocketAddr>> {
        let trackers = trackers
            .into_iter()
            .filter_map(|t| match Url::parse(&t) {
                Ok(parsed) => match parsed.scheme() {
                    "http" | "https" => Some(SupportedTracker::Http(parsed)),
                    "udp" => Some(SupportedTracker::Udp(parsed)),
                    _ => {
                        debug!("unsuppoted tracker URL: {}", t);
                        None
                    }
                },
                Err(e) => {
                    debug!("error parsing tracker URL {}: {}", t, e);
                    None
                }
            })
            .collect::<Vec<_>>();
        if trackers.is_empty() {
            return None;
        }

        let (tx, mut rx) = tokio::sync::mpsc::channel::<SocketAddr>(16);

        let s = async_stream::stream! {
            use futures::StreamExt;
            let mut futures = FuturesUnordered::new();
            for tracker in trackers {
                futures.push(task_single_tracker_monitor(&info, &tx, tracker))
            }
            while !(futures.is_empty()) {
                tokio::select! {
                    addr = rx.recv() => {
                        if let Some(addr) = addr {
                            yield addr;
                        }
                    }
                    e = futures.next(), if !futures.is_empty() => {
                        if let Some(Err(e)) = e {
                            debug!("error: {e}");
                        }
                    }
                }
            }
        };

        Some(s.boxed())
    }
}
