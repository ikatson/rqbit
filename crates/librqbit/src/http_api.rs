use std::sync::Arc;

use std::io::Write;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use warp::Filter;

use crate::torrent_state::TorrentState;

// This is just a stub for debugging, nothing useful here.
pub async fn make_and_run_http_api(state: Arc<TorrentState>) -> anyhow::Result<()> {
    let dump_haves = warp::path("haves").map({
        let state = state.clone();
        move || format!("{:?}", state.locked.read().chunks.get_have_pieces())
    });

    let dump_stats = warp::path("stats").map({
        let state = state.clone();
        let start_time = Instant::now();
        let initial_downloaded_and_checked =
            state.stats.downloaded_and_checked.load(Ordering::Relaxed);
        move || {
            let stats = &state.stats;
            let mut buf = Vec::new();
            writeln!(buf, "{:#?}", &stats).unwrap();
            writeln!(
                buf,
                "Average download time: {:?}",
                stats.average_piece_download_time()
            )
            .unwrap();

            // Poor mans download speed computation
            let elapsed = start_time.elapsed();
            let downloaded_bytes = state.stats.downloaded_and_checked.load(Ordering::Relaxed)
                - initial_downloaded_and_checked;
            let downloaded_mb = downloaded_bytes as f64 / 1024f64 / 1024f64;
            writeln!(
                buf,
                "Speed: {:.2}Mbps",
                downloaded_mb / elapsed.as_secs_f64()
            )
            .unwrap();

            buf
        }
    });

    let router = dump_haves.or(dump_stats);

    warp::serve(router).run(([127, 0, 0, 1], 3030)).await;
    Ok(())
}
