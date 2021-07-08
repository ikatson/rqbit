use std::net::SocketAddr;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::Serialize;
use std::time::{Duration, Instant};
use warp::Filter;

use crate::torrent_manager::TorrentManagerHandle;
use crate::torrent_state::StatsSnapshot;

struct Inner {
    startup_time: Instant,
    torrent_managers: RwLock<Vec<TorrentManagerHandle>>,
}

impl Inner {
    fn new() -> Self {
        Self {
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

impl Inner {
    fn mgr_handle(&self, idx: usize) -> Option<TorrentManagerHandle> {
        self.torrent_managers.read().get(idx).cloned()
    }

    fn api_torrent_list(&self) -> TorrentListResponse {
        TorrentListResponse {
            torrents: self
                .torrent_managers
                .read()
                .iter()
                .enumerate()
                .map(|(id, mgr)| TorrentListResponseItem {
                    id,
                    info_hash: hex::encode(mgr.torrent_state().info_hash()),
                })
                .collect(),
        }
    }

    fn api_torrent_details(&self, idx: usize) -> Option<TorrentDetailsResponse> {
        let handle = self.mgr_handle(idx)?;
        let info_hash = hex::encode(handle.torrent_state().info_hash());
        let files = handle
            .torrent_state()
            .info()
            .iter_filenames_and_lengths()
            .map(|(filename_it, length)| {
                let name = filename_it.to_string().ok();
                TorrentDetailsResponseFile { name, length }
            })
            .collect();
        Some(TorrentDetailsResponse { info_hash, files })
    }

    fn api_stats(&self, idx: usize) -> Option<StatsResponse> {
        let mgr = self.mgr_handle(idx)?;
        let snapshot = mgr.torrent_state().stats_snapshot();
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

    fn api_dump_haves(&self, idx: usize) -> Option<String> {
        let mgr = self.mgr_handle(idx)?;
        Some(format!(
            "{:?}",
            mgr.torrent_state().lock_read().chunks.get_have_pieces(),
        ))
    }
}

#[derive(Clone)]
pub struct HttpApi {
    inner: Arc<Inner>,
}

fn json_response<T: Serialize>(v: T) -> warp::reply::Response {
    let body = serde_json::to_string_pretty(&v).unwrap();
    let mut response = warp::reply::Response::new(body.into());
    response.headers_mut().insert(
        "content-type",
        warp::http::HeaderValue::from_static("application/json"),
    );
    response
}

fn not_found_response(idx: usize) -> warp::reply::Response {
    let mut response = warp::reply::Response::new(format!("torrent {} not found", idx).into());
    *response.status_mut() = warp::http::StatusCode::NOT_FOUND;
    response
}

fn json_or_404<T: Serialize>(idx: usize, v: Option<T>) -> warp::reply::Response {
    match v {
        Some(v) => json_response(v),
        None => not_found_response(idx),
    }
}

impl HttpApi {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner::new()),
        }
    }
    pub fn add_mgr(&self, handle: TorrentManagerHandle) -> usize {
        let mut g = self.inner.torrent_managers.write();
        let idx = g.len();
        g.push(handle);
        idx
    }

    // TODO: this is all for debugging, not even JSON.
    // After using this for a bit, not a big fan of warp.
    pub async fn make_http_api_and_run(self, addr: SocketAddr) -> anyhow::Result<()> {
        let inner = self.inner;

        let list = warp::path::end().map({
            let inner = inner.clone();
            move || json_response(inner.api_torrent_list())
        });

        let torrent_details = warp::path!(usize).map({
            let inner = inner.clone();
            move |idx| json_or_404(idx, inner.api_torrent_details(idx))
        });

        let dump_haves = warp::path!(usize / "haves").map({
            let inner = inner.clone();
            move |idx| json_or_404(idx, inner.api_dump_haves(idx))
        });

        let dump_stats = warp::path!(usize / "stats").map({
            let inner = inner.clone();
            move |idx| json_or_404(idx, inner.api_stats(idx))
        });

        let router = list.or(torrent_details).or(dump_haves).or(dump_stats);

        warp::serve(router).run(addr).await;
        Ok(())
    }
}

impl Default for HttpApi {
    fn default() -> Self {
        Self::new()
    }
}
