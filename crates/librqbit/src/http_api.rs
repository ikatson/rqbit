use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use dht::{Dht, DhtStats};
use serde::Serialize;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use warp::hyper::Body;
use warp::{Filter, Reply};

use crate::torrent_manager::TorrentManagerHandle;
use crate::torrent_state::StatsSnapshot;

struct ApiInternal {
    dht: Option<Dht>,
    startup_time: Instant,
    torrent_managers: RwLock<Vec<TorrentManagerHandle>>,
}

impl ApiInternal {
    fn new(dht: Option<Dht>) -> Self {
        Self {
            dht,
            startup_time: Instant::now(),
            torrent_managers: RwLock::new(Vec::new()),
        }
    }
}

#[derive(Serialize)]
struct Speed {
    mbps: f64,
    human_readable: String,
}

impl Speed {
    fn new(mbps: f64) -> Self {
        Self {
            mbps,
            human_readable: format!("{:.2}Mbps", mbps),
        }
    }
}

impl From<f64> for Speed {
    fn from(mbps: f64) -> Self {
        Self::new(mbps)
    }
}

#[derive(Serialize)]
struct TorrentListResponseItem {
    id: usize,
    info_hash: String,
}

#[derive(Serialize)]
struct TorrentListResponse {
    torrents: Vec<TorrentListResponseItem>,
}

#[derive(Serialize)]
struct TorrentDetailsResponseFile {
    name: Option<String>,
    length: u64,
}

#[derive(Serialize)]
struct TorrentDetailsResponse {
    info_hash: String,
    files: Vec<TorrentDetailsResponseFile>,
}

#[derive(Serialize)]
struct StatsResponse {
    snapshot: StatsSnapshot,
    average_piece_download_time: Option<Duration>,
    download_speed: Speed,
    all_time_download_speed: Speed,
    time_remaining: Option<Duration>,
}

impl ApiInternal {
    async fn mgr_handle(&self, idx: usize) -> Option<TorrentManagerHandle> {
        self.torrent_managers.read().await.get(idx).cloned()
    }

    async fn api_torrent_list(&self) -> TorrentListResponse {
        TorrentListResponse {
            torrents: self
                .torrent_managers
                .read()
                .await
                .iter()
                .enumerate()
                .map(|(id, mgr)| TorrentListResponseItem {
                    id,
                    info_hash: mgr.torrent_state().info_hash().as_string(),
                })
                .collect(),
        }
    }

    async fn api_torrent_details(&self, idx: usize) -> Option<TorrentDetailsResponse> {
        let handle = self.mgr_handle(idx).await?;
        let info_hash = handle.torrent_state().info_hash().as_string();
        let files = handle
            .torrent_state()
            .info()
            .iter_filenames_and_lengths()
            .unwrap()
            .map(|(filename_it, length)| {
                let name = filename_it.to_string().ok();
                TorrentDetailsResponseFile { name, length }
            })
            .collect();
        Some(TorrentDetailsResponse { info_hash, files })
    }

    async fn api_dht_stats(&self) -> Option<DhtStats> {
        if let Some(d) = self.dht.as_ref() {
            Some(d.stats().await)
        } else {
            None
        }
    }

    async fn api_stats(&self, idx: usize) -> Option<StatsResponse> {
        let mgr = self.mgr_handle(idx).await?;
        let snapshot = mgr.torrent_state().stats_snapshot().await;
        let estimator = mgr.speed_estimator();

        // Poor mans download speed computation
        let elapsed = self.startup_time.elapsed();
        let downloaded_bytes = snapshot.downloaded_and_checked_bytes;
        let downloaded_mb = downloaded_bytes as f64 / 1024f64 / 1024f64;

        Some(StatsResponse {
            average_piece_download_time: snapshot.average_piece_download_time(),
            snapshot,
            all_time_download_speed: (downloaded_mb / elapsed.as_secs_f64()).into(),
            download_speed: estimator.download_mbps().into(),
            time_remaining: estimator.time_remaining(),
        })
    }

    async fn api_dump_haves(&self, idx: usize) -> Option<String> {
        let mgr = self.mgr_handle(idx).await?;
        Some(format!(
            "{:?}",
            mgr.torrent_state()
                .lock_read()
                .await
                .chunks
                .get_have_pieces(),
        ))
    }
}

#[derive(Clone)]
pub struct HttpApi {
    inner: Arc<ApiInternal>,
}

fn json_response<T: Serialize>(v: T) -> warp::reply::Response {
    warp::reply::json(&v).into_response()
}

fn plaintext_response<B: Into<Body>>(body: B) -> warp::reply::Response {
    warp::reply::Response::new(body.into())
}

fn not_found_response<T: ToString>(body: T) -> warp::reply::Response {
    let mut response = warp::reply::Response::new(body.to_string().into());
    *response.status_mut() = warp::http::StatusCode::NOT_FOUND;
    response
}

fn torrent_not_found_response(idx: usize) -> warp::reply::Response {
    not_found_response(format!("torrent {} not found", idx))
}

fn json_or_404<T: Serialize>(idx: usize, v: Option<T>) -> warp::reply::Response {
    match v {
        Some(v) => json_response(v),
        None => torrent_not_found_response(idx),
    }
}

impl HttpApi {
    pub fn new(dht: Option<Dht>) -> Self {
        Self {
            inner: Arc::new(ApiInternal::new(dht)),
        }
    }
    pub async fn add_mgr(&self, handle: TorrentManagerHandle) -> usize {
        let mut g = self.inner.torrent_managers.write().await;
        let idx = g.len();
        g.push(handle);
        idx
    }

    pub async fn make_http_api_and_run(self, addr: SocketAddr) -> anyhow::Result<()> {
        let inner = self.inner;

        fn with_api(
            api: Arc<ApiInternal>,
        ) -> impl Filter<Extract = (Arc<ApiInternal>,), Error = std::convert::Infallible> + Clone
        {
            warp::any().map(move || api.clone())
        }

        let api_list = warp::path::end().map({
            let api_list = serde_json::json!({
                "apis": {
                    "GET /": "list all available APIs",
                    "GET /dht/stats": "DHT stats",
                    "GET /dht/table": "DHT routing table",
                    "GET /torrents": "List torrents (default torrent is 0)",
                    "GET /torrents/{index}": "Torrent details",
                    "GET /torrents/{index}/haves": "The bitfield of have pieces",
                    "GET /torrents/{index}/stats": "Torrent stats"
                }
            });
            move || json_response(&api_list)
        });

        let dht_stats = warp::path!("dht" / "stats")
            .and(with_api(inner.clone()))
            .and_then(|api: Arc<ApiInternal>| async move {
                Result::<_, Infallible>::Ok(match api.api_dht_stats().await {
                    Some(stats) => json_response(stats),
                    None => not_found_response("DHT is off"),
                })
            });

        let dht_routing_table = warp::path!("dht" / "table")
            .and(with_api(inner.clone()))
            .and_then(|api: Arc<ApiInternal>| async move {
                Result::<_, Infallible>::Ok(match api.dht.as_ref() {
                    Some(dht) => dht.with_routing_table(|r| json_response(r)).await,
                    None => not_found_response("DHT is off"),
                })
            });

        let torrent_list = warp::path!("torrents")
            .and(with_api(inner.clone()))
            .and_then(|api: Arc<ApiInternal>| async move {
                Result::<_, Infallible>::Ok(json_response(api.api_torrent_list().await))
            });

        let torrent_details = warp::path!("torrents" / usize)
            .and(with_api(inner.clone()))
            .and_then(|idx: usize, api: Arc<ApiInternal>| async move {
                Result::<_, Infallible>::Ok(json_or_404(idx, api.api_torrent_details(idx).await))
            });

        let torrent_dump_haves = warp::path!("torrents" / usize / "haves")
            .and(with_api(inner.clone()))
            .and_then(|idx: usize, api: Arc<ApiInternal>| async move {
                Result::<_, Infallible>::Ok(match api.api_dump_haves(idx).await {
                    Some(haves) => plaintext_response(haves),
                    None => torrent_not_found_response(idx),
                })
            });

        let torrent_dump_stats = warp::path!("torrents" / usize / "stats")
            .and(with_api(inner.clone()))
            .and_then(|idx: usize, api: Arc<ApiInternal>| async move {
                Result::<_, Infallible>::Ok(json_or_404(idx, api.api_stats(idx).await))
            });

        let router = api_list
            .or(torrent_list)
            .or(dht_stats)
            .or(dht_routing_table)
            .or(torrent_details)
            .or(torrent_dump_haves)
            .or(torrent_dump_stats);

        warp::serve(router).run(addr).await;
        Ok(())
    }
}
