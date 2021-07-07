use std::net::SocketAddr;
use std::sync::Arc;

use parking_lot::RwLock;
use std::io::Write;
use std::time::Instant;
use warp::Filter;

use crate::torrent_manager::TorrentManagerHandle;

enum Response {
    NotFound(usize),
    OkVec(Vec<u8>),
    OkString(String),
}

impl warp::Reply for Response {
    fn into_response(self) -> warp::reply::Response {
        match self {
            Response::NotFound(idx) => {
                let mut response = warp::reply::Response::new(warp::hyper::Body::from(format!(
                    "torrent {} not found",
                    idx
                )));
                *response.status_mut() = warp::http::StatusCode::NOT_FOUND;
                response
            }
            Response::OkVec(body) => warp::reply::Response::new(warp::hyper::Body::from(body)),
            Response::OkString(body) => warp::reply::Response::new(warp::hyper::Body::from(body)),
        }
    }
}

#[derive(Default)]
struct Inner {
    torrent_managers: RwLock<Vec<TorrentManagerHandle>>,
}

impl Inner {
    fn mgr_handle(&self, idx: usize) -> Option<TorrentManagerHandle> {
        self.torrent_managers.read().get(idx).cloned()
    }
}

#[derive(Clone, Default)]
pub struct HttpApi {
    inner: Arc<Inner>,
}

impl HttpApi {
    pub fn new() -> Self {
        Default::default()
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
            move || {
                let mut buf = Vec::<u8>::new();
                for (idx, handle) in inner.torrent_managers.read().iter().enumerate() {
                    writeln!(
                        buf,
                        "{}: {}\n",
                        idx,
                        hex::encode(handle.torrent_state().info_hash())
                    )
                    .unwrap();
                }
                Response::OkVec(buf)
            }
        });

        let dump_haves = warp::path!(usize / "haves").map({
            let inner = inner.clone();
            move |idx| {
                let mgr = match inner.mgr_handle(idx) {
                    Some(mgr) => mgr,
                    None => return Response::NotFound(idx),
                };
                return Response::OkString(format!(
                    "{:?}",
                    mgr.torrent_state().lock_read().chunks.get_have_pieces(),
                ));
            }
        });

        let dump_stats = warp::path!(usize / "stats").map({
            let inner = inner.clone();
            let start_time = Instant::now();
            move |idx| {
                let mgr = match inner.mgr_handle(idx) {
                    Some(mgr) => mgr,
                    None => return Response::NotFound(idx),
                };
                let snapshot = mgr.torrent_state().stats_snapshot();
                let estimator = mgr.speed_estimator();
                let mut buf = Vec::new();
                writeln!(buf, "{:#?}", &snapshot).unwrap();
                writeln!(
                    buf,
                    "Average download time: {:?}",
                    snapshot.average_piece_download_time()
                )
                .unwrap();

                // Poor mans download speed computation
                let elapsed = start_time.elapsed();
                let downloaded_bytes = snapshot.downloaded_and_checked_bytes;
                let downloaded_mb = downloaded_bytes as f64 / 1024f64 / 1024f64;
                writeln!(
                    buf,
                    "Total download speed over all time: {:.2}Mbps",
                    downloaded_mb / elapsed.as_secs_f64()
                )
                .unwrap();

                writeln!(buf, "Download speed: {:.2}Mbps", estimator.download_mbps()).unwrap();
                match estimator.time_remaining() {
                    Some(time) => {
                        writeln!(buf, "Time remaining: {:?}", time).unwrap();
                    }
                    None => {
                        writeln!(buf, "Time remaining: unknown").unwrap();
                    }
                }
                Response::OkVec(buf)
            }
        });

        let router = list.or(dump_haves).or(dump_stats);

        warp::serve(router).run(addr).await;
        Ok(())
    }
}
