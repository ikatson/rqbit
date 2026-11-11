//! BitTorrent v2 (BEP 52) against libtorrent, for v2-only and hybrid torrents: rqbit downloads
//! from libtorrent and libtorrent downloads from rqbit. In both directions the downloader is
//! given the `.torrent` without its `piece layers`, or a magnet link (`btmh`, with `btih` for
//! a hybrid torrent) so that it reads the info dictionary from the seeder first, and it has to
//! ask the seeder for the layers with BEP 52 hash requests: the seeder's answers are checked
//! against the pieces roots by an independent implementation.

use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, ensure};
use bytes::Bytes;
use librqbit_core::torrent_metainfo::torrent_from_bytes;
use serde_json::json;
use tempfile::TempDir;

use crate::{
    Session,
    tests::{
        e2e_v2::{PIECE_LEN, SessionSetup, add, completed, new_session, sample_files},
        e2e_v2_magnet::add_magnet,
        fake_peer::run,
        v2_torrent::V2Files,
    },
    torrent_state::ManagedTorrentHandle,
};

use super::{Libtorrent, port_of};

const WAIT: Duration = Duration::from_secs(60);
const NAME: &str = "v2";

// Seeds the content under the save path argv[2], described by the torrent file argv[1].
const SEED_PY: &str = r#"
def main():
    session = make_session()
    info = lt.torrent_info(sys.argv[1])
    params = started_params(info, sys.argv[2])
    params.flags |= lt.torrent_flags.seed_mode
    handle = session.add_torrent(params)
    emit(
        "listening",
        port=wait_listen_port(session),
        version=lt.version,
        v2=info.info_hashes().has_v2(),
    )
    while True:
        cmd = next_command(timeout=0.2)
        if cmd and cmd["cmd"] == "quit":
            emit("uploaded", bytes=handle.status().total_payload_upload)
            return
        for a in pop_alerts(session, wait=0):
            if isinstance(a, lt.peer_error_alert):
                log("seeder:", a.message())

run(main)
"#;

// Downloads the torrent argv[1] (a file without its piece layers, or a magnet link) from the
// peer at 127.0.0.1:argv[2] into the current folder.
const LEECH_PY: &str = r#"
def main():
    session = make_session()
    if sys.argv[1].startswith("magnet:"):
        params = lt.parse_magnet_uri(sys.argv[1])
        params.save_path = "."
        params.flags &= ~(lt.torrent_flags.auto_managed | lt.torrent_flags.paused)
    else:
        params = started_params(lt.torrent_info(sys.argv[1]), ".")
    handle = session.add_torrent(params)
    handle.connect_peer(("127.0.0.1", int(sys.argv[2])))
    failed = 0
    while True:
        cmd = next_command(timeout=0.2)
        if cmd and cmd["cmd"] == "quit":
            return
        for a in pop_alerts(session, wait=0):
            if isinstance(a, lt.hash_failed_alert):
                failed += 1
            if isinstance(a, (lt.peer_error_alert, lt.peer_disconnected_alert)):
                log("leech:", a.message())
        status = handle.status()
        if status.is_seeding:
            emit(
                "finished",
                downloaded=status.total_payload_download,
                hash_failures=failed,
                v2=handle.torrent_file().info_hashes().has_v2(),
            )
            return

run(main)
"#;

fn full_torrent(files: &V2Files, hybrid: bool) -> Bytes {
    Bytes::from(files.torrent(NAME, PIECE_LEN, hybrid).to_bytes())
}

fn torrent_without_piece_layers(files: &V2Files, hybrid: bool) -> Bytes {
    let mut t = files.torrent(NAME, PIECE_LEN, hybrid);
    t.remove("piece layers")
        .expect("the test torrent has piece layers");
    Bytes::from(t.to_bytes())
}

/// The magnet link of a torrent: `btmh`, and `btih` too for a hybrid torrent.
fn magnet_of(files: &V2Files, hybrid: bool) -> anyhow::Result<String> {
    let full = full_torrent(files, hybrid);
    let parsed = torrent_from_bytes(&full)?;
    let v2 = parsed.info_hash_v2.context("no v2 hash")?;
    let mut magnet = format!("magnet:?xt=urn:btmh:1220{}", v2.as_string());
    if hybrid {
        magnet.push_str(&format!("&xt=urn:btih:{}", parsed.info_hash.as_string()));
    }
    magnet.push_str(&format!("&dn={NAME}"));
    Ok(magnet)
}

async fn rqbit_session(dir: &Path) -> anyhow::Result<Arc<Session>> {
    new_session(
        dir,
        SessionSetup {
            listen: true,
            ..Default::default()
        },
    )
    .await
}

/// libtorrent seeds `files`; rqbit, given the torrent without piece layers or a magnet link,
/// fetches the metadata and the layers from libtorrent and downloads everything.
async fn rqbit_downloads_from_libtorrent(hybrid: bool, magnet: bool) -> anyhow::Result<()> {
    let files = sample_files(if hybrid { 52 } else { 51 } + if magnet { 10 } else { 0 });
    let work = TempDir::with_prefix("rqbit_interop_v2")?;
    let torrent_file = work.path().join("full.torrent");
    tokio::fs::write(&torrent_file, full_torrent(&files, hybrid)).await?;
    let seed_root = work.path().join("seed");
    files.write_to(&seed_root.join(NAME));

    let mut lt = Libtorrent::spawn(
        SEED_PY,
        &[
            torrent_file.to_str().context("utf-8 path")?,
            seed_root.to_str().context("utf-8 path")?,
        ],
    )
    .await?;
    let listening = lt.expect("listening", WAIT).await?;
    tracing::info!(%listening, "libtorrent seeds");
    ensure!(listening["v2"] == json!(true), "libtorrent sees no v2 hash");
    let port = port_of(&listening, "port")?;

    let client_dir = work.path().join("client");
    let session = rqbit_session(&client_dir).await?;
    let out = client_dir.join("out");
    let peers = vec![([127, 0, 0, 1], port).into()];
    let handle = if magnet {
        add_magnet(&session, &magnet_of(&files, hybrid)?, &out, peers).await?
    } else {
        add(
            &session,
            torrent_without_piece_layers(&files, hybrid),
            &out,
            peers,
        )
        .await?
    };
    completed(&handle, WAIT).await?;
    files.assert_equal_to(&out);
    let live = handle.live().context("not live")?;
    ensure!(
        live.hash_requests_sent() >= 1,
        "the piece layers were never asked of libtorrent"
    );
    ensure!(
        handle
            .metadata
            .load()
            .as_ref()
            .is_some_and(|m| m.info.has_all_piece_layers()),
        "the piece layers were not learned"
    );

    lt.send(&json!({"cmd": "quit"})).await?;
    let uploaded = lt.expect("uploaded", WAIT).await?;
    ensure!(
        uploaded["bytes"].as_u64() >= Some(files.total_len()),
        "libtorrent did not upload the files: {uploaded}"
    );
    session.stop().await;
    Ok(())
}

fn hash_requests_received(handle: &ManagedTorrentHandle) -> u64 {
    handle.live().map_or(0, |l| l.hash_requests_received())
}

/// rqbit seeds `files`; libtorrent, given the torrent without piece layers or a magnet link,
/// asks rqbit for the metadata and the layers and downloads everything.
async fn libtorrent_downloads_from_rqbit(hybrid: bool, magnet: bool) -> anyhow::Result<()> {
    let files = sample_files(if hybrid { 54 } else { 53 } + if magnet { 10 } else { 0 });
    let work = TempDir::with_prefix("rqbit_interop_v2")?;
    let seed_dir = work.path().join("seed");
    files.write_to(&seed_dir);
    let session = rqbit_session(&work.path().join("session")).await?;
    let handle = add(&session, full_torrent(&files, hybrid), &seed_dir, vec![]).await?;
    completed(&handle, WAIT).await?;
    let port = session
        .listen_addr()
        .context("rqbit does not listen")?
        .port();

    let torrent_file = work.path().join("stripped.torrent");
    tokio::fs::write(&torrent_file, torrent_without_piece_layers(&files, hybrid)).await?;
    let source = if magnet {
        magnet_of(&files, hybrid)?
    } else {
        torrent_file.to_str().context("utf-8 path")?.to_owned()
    };
    let mut lt = Libtorrent::spawn(LEECH_PY, &[&source, &port.to_string()]).await?;
    let finished = lt.expect("finished", WAIT).await?;
    tracing::info!(%finished, "libtorrent downloaded");
    ensure!(finished["v2"] == json!(true), "libtorrent sees no v2 hash");
    ensure!(
        finished["hash_failures"] == json!(0),
        "libtorrent failed pieces: {finished}"
    );
    files.assert_equal_to(&lt.dir().join(NAME));
    ensure!(
        hash_requests_received(&handle) >= 1,
        "libtorrent never asked rqbit for the piece layers"
    );
    session.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_LIBTORRENT_PYTHON"]
async fn test_interop_v2_rqbit_downloads_from_libtorrent() -> anyhow::Result<()> {
    run(rqbit_downloads_from_libtorrent(false, false)).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_LIBTORRENT_PYTHON"]
async fn test_interop_hybrid_rqbit_downloads_from_libtorrent() -> anyhow::Result<()> {
    run(rqbit_downloads_from_libtorrent(true, false)).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_LIBTORRENT_PYTHON"]
async fn test_interop_v2_libtorrent_downloads_from_rqbit() -> anyhow::Result<()> {
    run(libtorrent_downloads_from_rqbit(false, false)).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_LIBTORRENT_PYTHON"]
async fn test_interop_hybrid_libtorrent_downloads_from_rqbit() -> anyhow::Result<()> {
    run(libtorrent_downloads_from_rqbit(true, false)).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_LIBTORRENT_PYTHON"]
async fn test_interop_v2_magnet_rqbit_downloads_from_libtorrent() -> anyhow::Result<()> {
    run(rqbit_downloads_from_libtorrent(false, true)).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_LIBTORRENT_PYTHON"]
async fn test_interop_hybrid_magnet_rqbit_downloads_from_libtorrent() -> anyhow::Result<()> {
    run(rqbit_downloads_from_libtorrent(true, true)).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_LIBTORRENT_PYTHON"]
async fn test_interop_v2_magnet_libtorrent_downloads_from_rqbit() -> anyhow::Result<()> {
    run(libtorrent_downloads_from_rqbit(false, true)).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_LIBTORRENT_PYTHON"]
async fn test_interop_hybrid_magnet_libtorrent_downloads_from_rqbit() -> anyhow::Result<()> {
    run(libtorrent_downloads_from_rqbit(true, true)).await
}
