//! Downloads from libtorrent and uploads to it: the two directions every other interop test
//! builds on.

use std::{net::SocketAddr, time::Duration};

use anyhow::Context;
use serde_json::json;
use tokio::time::timeout;

use crate::tests::fake_peer::{ClientOpts, SeederOpts, run, start_client, start_seeder};

use super::{Libtorrent, port_of};

const WAIT: Duration = Duration::from_secs(30);

// Seeds the content in the folder argv[2], described by the torrent file argv[1].
const SEED_PY: &str = r#"
def main():
    session = make_session()
    info = lt.torrent_info(sys.argv[1])
    params = started_params(info, save_path_for(info, sys.argv[2]))
    params.flags |= lt.torrent_flags.seed_mode
    handle = session.add_torrent(params)
    emit("listening", port=wait_listen_port(session), version=lt.version)
    while True:
        cmd = next_command(timeout=0.2)
        if cmd and cmd["cmd"] == "quit":
            emit("uploaded", bytes=handle.status().total_payload_upload)
            return

run(main)
"#;

// Downloads the torrent argv[1] from the peer at 127.0.0.1:argv[2] into the current folder.
const LEECH_PY: &str = r#"
def main():
    session = make_session()
    params = started_params(lt.torrent_info(sys.argv[1]), ".")
    handle = session.add_torrent(params)
    handle.connect_peer(("127.0.0.1", int(sys.argv[2])))
    while True:
        cmd = next_command(timeout=0.2)
        if cmd and cmd["cmd"] == "quit":
            return
        status = handle.status()
        if status.is_seeding:
            emit(
                "finished",
                downloaded=status.total_payload_download,
                file=first_file_path(params.ti),
            )
            return

run(main)
"#;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_LIBTORRENT_PYTHON"]
async fn test_interop_download_from_libtorrent() -> anyhow::Result<()> {
    run(async {
        // Only the content and the torrent file of this fixture are used: libtorrent seeds.
        let seed = start_seeder(SeederOpts::default()).await?;
        seed.session.stop().await;
        let content_dir = seed._dir.path();
        let torrent_dir = tempfile::tempdir()?;
        let torrent = torrent_dir.path().join("interop.torrent");
        tokio::fs::write(&torrent, &seed.torrent_bytes).await?;

        let mut lt = Libtorrent::spawn(
            SEED_PY,
            &[
                torrent.to_str().context("path")?,
                content_dir.to_str().context("path")?,
            ],
        )
        .await?;
        let listening = lt.expect("listening", WAIT).await?;
        let addr = SocketAddr::from(([127, 0, 0, 1], port_of(&listening, "port")?));

        let client = start_client(
            &seed,
            ClientOpts {
                initial_peers: vec![addr],
                ..Default::default()
            },
        )
        .await?;
        timeout(WAIT, client.handle.wait_until_completed())
            .await
            .context("the download from libtorrent timed out")??;
        let downloaded = tokio::fs::read(client._dir.path().join("0.data")).await?;
        assert!(downloaded == seed.content, "content differs");

        lt.send(&json!({"cmd": "quit"})).await?;
        let uploaded = lt.expect("uploaded", WAIT).await?;
        assert_eq!(
            uploaded.get("bytes").and_then(|v| v.as_u64()),
            Some(seed.content.len() as u64)
        );
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_LIBTORRENT_PYTHON"]
async fn test_interop_upload_to_libtorrent() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let torrent_dir = tempfile::tempdir()?;
        let torrent = torrent_dir.path().join("interop.torrent");
        tokio::fs::write(&torrent, &seed.torrent_bytes).await?;

        let port = seed.addr().port().to_string();
        let mut lt =
            Libtorrent::spawn(LEECH_PY, &[torrent.to_str().context("path")?, &port]).await?;
        let finished = lt.expect("finished", WAIT).await?;
        assert_eq!(
            finished.get("downloaded").and_then(|v| v.as_u64()),
            Some(seed.content.len() as u64)
        );
        let file = finished
            .get("file")
            .and_then(|v| v.as_str())
            .context("no file in the event")?;
        let downloaded = tokio::fs::read(file).await?;
        assert!(downloaded == seed.content, "content differs");
        Ok(())
    })
    .await
}
