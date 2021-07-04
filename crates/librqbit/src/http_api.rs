use std::sync::Arc;

use librqbit_core::speed_estimator::SpeedEstimator;
use std::io::Write;
use std::time::Instant;
use warp::Filter;

use crate::torrent_state::TorrentState;

// This is just a stub for debugging.
// A real http api would know about ALL torrents we are downloading, not just one.
pub async fn make_and_run_http_api(
    state: Arc<TorrentState>,
    estimator: Arc<SpeedEstimator>,
) -> anyhow::Result<()> {
    let dump_haves = warp::path("haves").map({
        let state = state.clone();
        move || format!("{:?}", state.lock_read().chunks.get_have_pieces())
    });

    let dump_stats = warp::path("stats").map({
        let state = state.clone();
        let start_time = Instant::now();
        let initial_downloaded_and_checked = state.stats_snapshot().downloaded_and_checked_bytes;
        move || {
            let snapshot = state.stats_snapshot();
            let mut buf = Vec::new();
            writeln!(buf, "{:#?}", state.stats_snapshot()).unwrap();
            writeln!(
                buf,
                "Average download time: {:?}",
                snapshot.average_piece_download_time()
            )
            .unwrap();

            // Poor mans download speed computation
            let elapsed = start_time.elapsed();
            let downloaded_bytes =
                snapshot.downloaded_and_checked_bytes - initial_downloaded_and_checked;
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
            buf
        }
    });

    let router = dump_haves.or(dump_stats);

    warp::serve(router).run(([127, 0, 0, 1], 3030)).await;
    Ok(())
}
