use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::bail;
use anyhow::Context;
use futures::Stream;
use librqbit_core::spawn_utils::spawn_with_cancel;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use tracing::error_span;
use tracing::info;
use tracing::trace;
use url::Url;

use crate::tracker_comms_http;
use crate::tracker_comms_udp;
use librqbit_core::hash_id::Id20;

pub struct TrackerComms {
    info_hash: Id20,
    peer_id: Id20,
    stats: Box<dyn TorrentStatsProvider>,
    force_tracker_interval: Option<Duration>,
    cancellation_token: CancellationToken,
    tx: Sender,
    tcp_listen_port: Option<u16>,
}

#[derive(Default)]
pub struct TrackerCommsStats {
    pub uploaded_bytes: u64,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
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

impl TrackerComms {
    pub fn start(
        info_hash: Id20,
        peer_id: Id20,
        trackers: Vec<String>,
        stats: Box<dyn TorrentStatsProvider>,
        force_interval: Option<Duration>,
        cancellation_token: CancellationToken,
        tcp_listen_port: Option<u16>,
    ) -> Option<impl Stream<Item = SocketAddr> + Send + Sync + Unpin + 'static> {
        let (tx, rx) = tokio::sync::mpsc::channel::<SocketAddr>(16);
        let comms = Arc::new(Self {
            info_hash,
            peer_id,
            stats,
            force_tracker_interval: force_interval,
            cancellation_token,
            tx,
            tcp_listen_port,
        });
        let mut added = false;
        for tracker in trackers {
            if let Err(e) = comms.clone().add_tracker(&tracker) {
                info!(tracker = tracker, "error adding tracker: {:#}", e)
            } else {
                added = true;
            }
        }
        if !added {
            return None;
        }
        Some(tokio_stream::wrappers::ReceiverStream::new(rx))
    }

    fn add_tracker(self: Arc<Self>, tracker: &str) -> anyhow::Result<()> {
        if tracker.starts_with("http://") || tracker.starts_with("https://") {
            spawn_with_cancel(
                error_span!(
                    parent: None,
                    "http_tracker",
                    tracker = tracker,
                    info_hash = ?self.info_hash
                ),
                self.cancellation_token.clone(),
                {
                    let comms = self;
                    let url = Url::parse(tracker).context("can't parse URL")?;
                    async move { comms.task_single_tracker_monitor_http(url).await }
                },
            );
        } else if tracker.starts_with("udp://") {
            spawn_with_cancel(
                error_span!(parent: None, "udp_tracker", tracker = tracker, info_hash = ?self.info_hash),
                self.cancellation_token.clone(),
                {
                    let comms = self;
                    let url = Url::parse(tracker).context("can't parse URL")?;
                    async move { comms.task_single_tracker_monitor_udp(url).await }
                },
            );
        } else {
            bail!("unsupported tracker url {}", tracker)
        }
        Ok(())
    }

    async fn task_single_tracker_monitor_http(
        self: Arc<Self>,
        mut tracker_url: Url,
    ) -> anyhow::Result<()> {
        let mut event = Some(tracker_comms_http::TrackerRequestEvent::Started);
        loop {
            let stats = self.stats.get();
            let request = tracker_comms_http::TrackerRequest {
                info_hash: self.info_hash,
                peer_id: self.peer_id,
                port: 6778,
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

            match self.tracker_one_request_http(tracker_url.clone()).await {
                Ok(interval) => {
                    event = None;
                    let interval = self
                        .force_tracker_interval
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

    async fn tracker_one_request_http(&self, tracker_url: Url) -> anyhow::Result<u64> {
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
            self.tx.send(peer).await?;
        }
        Ok(response.interval)
    }

    async fn task_single_tracker_monitor_udp(&self, url: Url) -> anyhow::Result<()> {
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

            let stats = self.stats.get();
            let request = AnnounceFields {
                info_hash: self.info_hash,
                peer_id: self.peer_id,
                downloaded: stats.downloaded_bytes,
                left: stats.get_left_to_download_bytes(),
                uploaded: stats.uploaded_bytes,
                event: EVENT_NONE,
                key: 0, // whatever that is?
                port: self.tcp_listen_port.unwrap_or(0),
            };

            match requester.announce(request).await {
                Ok(response) => {
                    trace!(len = response.addrs.len(), "received announce response");
                    for addr in response.addrs {
                        self.tx
                            .send(SocketAddr::V4(addr))
                            .await
                            .context("rx closed")?;
                    }
                    let new_interval = response.interval.max(5);
                    let new_interval = Duration::from_secs(new_interval as u64);
                    sleep_interval = Some(self.force_tracker_interval.unwrap_or(new_interval));
                }
                Err(e) => {
                    debug!(url = ?url, "error reading announce response: {e:#}");
                    if sleep_interval.is_none() {
                        sleep_interval = Some(
                            self.force_tracker_interval
                                .unwrap_or(Duration::from_secs(60)),
                        );
                    }
                }
            }
        }
    }
}
