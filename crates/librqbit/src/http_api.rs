use std::sync::Arc;

use warp::Filter;

use crate::torrent_state::TorrentState;

// This is just a stub for debugging, nothing useful here.
pub async fn make_and_run_http_api(state: Arc<TorrentState>) -> anyhow::Result<()> {
    let dump_haves = warp::path("haves")
        .map(move || format!("{:?}", state.locked.read().chunks.get_have_pieces()));
    warp::serve(dump_haves).run(([127, 0, 0, 1], 3030)).await;
    Ok(())
}
