use anyhow::Context;
use dht::{Dht, DhtStats};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use warp::hyper::body::Bytes;
use warp::hyper::Body;
use warp::Filter;

use crate::session::{AddTorrentOptions, Session};
use crate::torrent_manager::TorrentManagerHandle;
use crate::torrent_state::StatsSnapshot;

struct ApiInternal {
    dht: Option<Dht>,
    startup_time: Instant,
    torrent_managers: RwLock<Vec<TorrentManagerHandle>>,
    session: Arc<Session>,
}

impl ApiInternal {
    fn new(session: Arc<Session>) -> Self {
        Self {
            dht: session.get_dht(),
            startup_time: Instant::now(),
            torrent_managers: RwLock::new(Vec::new()),
            session,
        }
    }

    fn add_mgr(&self, handle: TorrentManagerHandle) -> usize {
        let mut g = self.torrent_managers.write();
        let idx = g.len();
        g.push(handle);
        idx
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
                    info_hash: mgr.torrent_state().info_hash().as_string(),
                })
                .collect(),
        }
    }

    fn api_torrent_details(&self, idx: usize) -> Option<TorrentDetailsResponse> {
        let handle = self.mgr_handle(idx)?;
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

    async fn api_add_torrent(
        &self,
        url: String,
        opts: Option<AddTorrentOptions>,
    ) -> anyhow::Result<usize> {
        let handle = self
            .session
            .add_torrent(&url, opts)
            .await
            .context("error adding torrent")?
            .context("expected session.add_torrent() to return a handle")?;
        Ok(self.add_mgr(handle))
    }

    fn api_dht_stats(&self) -> Option<DhtStats> {
        self.dht.as_ref().map(|d| d.stats())
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
    inner: Arc<ApiInternal>,
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

fn plaintext_response<B: Into<Body>>(body: B) -> warp::reply::Response {
    warp::reply::Response::new(body.into())
}

fn not_found_response(body: String) -> warp::reply::Response {
    let mut response = warp::reply::Response::new(body.into());
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

#[derive(Serialize, Deserialize)]
pub struct TorrentAddQueryParams {
    pub overwrite: Option<bool>,
    pub output_folder: Option<String>,
    pub only_files_regex: Option<String>,
}

impl HttpApi {
    pub fn new(session: Arc<Session>) -> Self {
        Self {
            inner: Arc::new(ApiInternal::new(session)),
        }
    }
    pub fn add_mgr(&self, handle: TorrentManagerHandle) -> usize {
        self.inner.add_mgr(handle)
    }

    pub async fn make_http_api_and_run(self, addr: SocketAddr) -> anyhow::Result<()> {
        let inner = self.inner;

        let api_list = warp::path::end().map({
            let api_list = serde_json::json!({
                "apis": {
                    "GET /": "list all available APIs",
                    "GET /dht/stats": "DHT stats",
                    "GET /dht/table": "DHT routing table",
                    "GET /torrents": "List torrents (default torrent is 0)",
                    "GET /torrents/{index}": "Torrent details",
                    "GET /torrents/{index}/haves": "The bitfield of have pieces",
                    "GET /torrents/{index}/stats": "Torrent stats",
                    // This is kind of not secure as it just reads any local file that it has access to,
                    // or any URL, but whatever, ok for our purposes / thread model.
                    "POST /torrents/": "Add a torrent here. magnet: or http:// or a local file."
                },
                "server": "rqbit",
            });
            move || json_response(&api_list)
        });

        let dht_stats = warp::path!("dht" / "stats").map({
            let inner = inner.clone();
            move || match inner.api_dht_stats() {
                Some(stats) => json_response(stats),
                None => not_found_response("DHT is off".into()),
            }
        });

        let dht_routing_table = warp::path!("dht" / "table").map({
            let inner = inner.clone();

            // clippy suggests something that doesn't work here.
            #[allow(clippy::redundant_closure)]
            move || match inner.dht.as_ref() {
                Some(dht) => dht.with_routing_table(|r| json_response(r)),
                None => not_found_response("DHT is off".into()),
            }
        });

        let torrent_list = warp::get().and(warp::path("torrents")).map({
            let inner = inner.clone();
            move || json_response(inner.api_torrent_list())
        });

        let torrent_add = warp::post()
            .and(warp::path("torrents"))
            .and(warp::body::bytes())
            .and(warp::query())
            .and_then({
                let inner = inner.clone();
                use warp::http::Response;
                fn make_response(status: u16, body: String) -> Response<String> {
                    Response::builder().status(status).body(body).unwrap()
                }
                move |body: Bytes, params: TorrentAddQueryParams| {
                    let inner = inner.clone();
                    async move {
                        let url = match String::from_utf8(body.to_vec()) {
                            Ok(str) => str,
                            Err(_) => {
                                return Ok::<_, warp::Rejection>(make_response(
                                    400,
                                    "invalid utf-8".into(),
                                ))
                            }
                        };
                        let opts = AddTorrentOptions {
                            overwrite: params.overwrite.unwrap_or(false),
                            only_files_regex: params.only_files_regex,
                            output_folder: params.output_folder,
                            ..Default::default()
                        };
                        let idx = inner
                            .api_add_torrent(url, Some(opts))
                            .await
                            .context("error calling HttpApi::api_add_torrent");
                        match idx {
                            Ok(idx) => {
                                return Ok(make_response(200, format!("{}", idx)));
                            }
                            Err(e) => return Ok(make_response(400, format!("{:#}", e))),
                        }
                    }
                }
            });

        let torrent_details = warp::path!("torrents" / usize).map({
            let inner = inner.clone();
            move |idx| json_or_404(idx, inner.api_torrent_details(idx))
        });

        let torrent_dump_haves = warp::path!("torrents" / usize / "haves").map({
            let inner = inner.clone();
            move |idx| match inner.api_dump_haves(idx) {
                Some(haves) => plaintext_response(haves),
                None => torrent_not_found_response(idx),
            }
        });

        let torrent_dump_stats = warp::path!("torrents" / usize / "stats").map({
            let inner = inner.clone();
            move |idx| json_or_404(idx, inner.api_stats(idx))
        });

        let router = api_list
            .or(dht_stats)
            .or(dht_routing_table)
            .or(torrent_details)
            .or(torrent_dump_haves)
            .or(torrent_dump_stats)
            .or(torrent_add)
            .or(torrent_list);

        warp::serve(router).run(addr).await;
        Ok(())
    }
}
