//! Web seeds (BEP 19) against libtorrent, with the in-process HTTP server of
//! [`crate::tests::web_seed_server`]: rqbit downloads a torrent that libtorrent made with
//! `add_url_seed` from the server alone, and libtorrent downloads a torrent whose `url-list`
//! rqbit parsed from the same kind of server, which serves the files at the URLs rqbit maps
//! them to. Both directions working proves the mapping is the standard one.

use std::time::Duration;

use anyhow::Context;
use bytes::Bytes;
use serde_json::{Value, json};

use crate::{
    AddTorrentOptions,
    tests::{
        e2e_webseed::{
            Client, Entry, PIECE, Torrent, multi_file, only_seed, run, single_file, wait_completed,
        },
        web_seed_server::WebSeedServer,
    },
    torrent_from_bytes,
};

use super::Libtorrent;

const WAIT: Duration = Duration::from_secs(60);

// Makes a v1 torrent of the file or folder argv[1] with `piece_size` argv[2] and the web seed
// argv[3] added with `add_url_seed`, and writes it to `created.torrent`.
const CREATE_PY: &str = r#"
def main():
    content = os.path.abspath(sys.argv[1])
    fs = lt.file_storage()
    lt.add_files(fs, content)
    t = lt.create_torrent(fs, int(sys.argv[2]), flags=lt.create_torrent.v1_only)
    t.add_url_seed(sys.argv[3])
    lt.set_piece_hashes(t, os.path.dirname(content))
    path = os.path.abspath("created.torrent")
    with open(path, "wb") as f:
        f.write(lt.bencode(t.generate()))
    emit("created", file=path)

run(main)
"#;

// Downloads the torrent argv[1] into the current folder from its web seeds alone: the session
// finds no peers.
const LEECH_PY: &str = r#"
def main():
    session = make_session()
    info = lt.torrent_info(sys.argv[1])
    emit("web_seeds", urls=[w["url"] for w in info.web_seeds()])
    handle = session.add_torrent(started_params(info, "."))
    while True:
        for alert in session.pop_alerts():
            if isinstance(alert, lt.url_seed_alert):
                log("url seed:", alert.message())
        cmd = next_command(timeout=0.2)
        if cmd and cmd["cmd"] == "quit":
            return
        status = handle.status()
        if status.is_seeding:
            emit("finished", downloaded=status.total_payload_download)
            return

run(main)
"#;

/// rqbit downloads, from the server alone, the torrent libtorrent makes of `torrent`'s files
/// with `add_url_seed(server URL)`.
async fn rqbit_downloads_what_libtorrent_made(torrent: Torrent) -> anyhow::Result<()> {
    let server = WebSeedServer::start(torrent.server_files()).await?;
    let content = tempfile::tempdir()?;
    // libtorrent names the torrent after the file or folder it is made of.
    for i in 0..torrent.files.len() {
        let path = if torrent.single {
            torrent.disk_path(content.path(), i)
        } else {
            torrent.disk_path(&content.path().join(&torrent.name), i)
        };
        std::fs::create_dir_all(path.parent().context("parent")?)?;
        std::fs::write(&path, &torrent.files[i].1)?;
    }
    let root = content.path().join(&torrent.name);
    let mut lt = Libtorrent::spawn(
        CREATE_PY,
        &[
            root.to_str().context("path")?,
            &PIECE.to_string(),
            &server.url(),
        ],
    )
    .await?;
    let created = lt.expect("created", WAIT).await?;
    let file = created
        .get("file")
        .and_then(Value::as_str)
        .with_context(|| format!("no file in {created}"))?;
    let made = Torrent {
        bytes: Bytes::from(std::fs::read(file)?),
        ..torrent
    };
    let parsed = torrent_from_bytes(&made.bytes)?;
    let url_list: Vec<&[u8]> = parsed.url_list.iter().map(|u| u.as_ref()).collect();
    assert_eq!(url_list, vec![server.url().as_bytes()]);

    let client = Client::start(&made, AddTorrentOptions::default()).await?;
    wait_completed(&client.handle).await?;
    made.check_files(&client.out)?;
    let seed = only_seed(&client.handle)?;
    assert_eq!(seed.url, server.url());
    assert_eq!(seed.downloaded_bytes, made.content_len());
    for s in server.log() {
        assert_eq!(s.status, 206, "{s:?}");
    }
    client.session.stop().await;
    Ok(())
}

/// libtorrent downloads `torrent`, whose `url-list` is the server URL, from the server alone.
async fn libtorrent_downloads_what_rqbit_parsed(
    build: impl Fn(&[&str]) -> Torrent,
) -> anyhow::Result<()> {
    let server = WebSeedServer::start(build(&[]).server_files()).await?;
    let url = server.url();
    let torrent = build(&[&url]);
    let parsed = torrent_from_bytes(&torrent.bytes)?;
    let url_list: Vec<&[u8]> = parsed.url_list.iter().map(|u| u.as_ref()).collect();
    assert_eq!(url_list, vec![url.as_bytes()]);

    let dir = tempfile::tempdir()?;
    let file = dir.path().join("seeded.torrent");
    std::fs::write(&file, &torrent.bytes)?;
    let mut lt = Libtorrent::spawn(LEECH_PY, &[file.to_str().context("path")?]).await?;
    let seeds = lt.expect("web_seeds", WAIT).await?;
    assert_eq!(seeds.get("urls"), Some(&json!([url])), "{seeds}");
    lt.expect("finished", WAIT).await?;

    // libtorrent keeps the files of a multi-file torrent in a folder named after it.
    let out = if torrent.single {
        lt.dir()
    } else {
        lt.dir().join(&torrent.name)
    };
    torrent.check_files(&out)?;
    let log = server.log();
    assert!(!log.is_empty());
    for s in &log {
        assert!(
            (0..torrent.files.len()).any(|i| s.path == torrent.url_path(i)),
            "libtorrent asked for {s:?}, which is not where rqbit looks for a file"
        );
        assert_eq!(s.status, 206, "{s:?}");
    }
    Ok(())
}

fn nested(name: &str, url_list: &[&str]) -> Torrent {
    multi_file(
        name,
        &[
            Entry::File(&["a b.bin"], 3 * PIECE - 100),
            Entry::Pad(100),
            Entry::File(&["dir one", "inner", "100% ü+.txt"], 2 * PIECE + 5),
            Entry::Pad(PIECE - 5),
            Entry::File(&["dir one", "c"], 1000),
        ],
        url_list,
    )
}

/// Only the files of `nested`, without its padding: libtorrent adds its own.
fn nested_content(name: &str) -> Torrent {
    multi_file(
        name,
        &[
            Entry::File(&["a b.bin"], 3 * PIECE - 100),
            Entry::File(&["dir one", "inner", "100% ü+.txt"], 2 * PIECE + 5),
            Entry::File(&["dir one", "c"], 1000),
        ],
        &[],
    )
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_LIBTORRENT_PYTHON"]
async fn test_interop_rqbit_downloads_single_file_from_libtorrent_url_seed() -> anyhow::Result<()> {
    run(async {
        rqbit_downloads_what_libtorrent_made(single_file("lt single ü.bin", 9 * PIECE + 77, &[]))
            .await
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_LIBTORRENT_PYTHON"]
async fn test_interop_rqbit_downloads_multi_file_from_libtorrent_url_seed() -> anyhow::Result<()> {
    run(rqbit_downloads_what_libtorrent_made(nested_content(
        "lt multi",
    )))
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_LIBTORRENT_PYTHON"]
async fn test_interop_libtorrent_downloads_single_file_from_rqbit_url_list() -> anyhow::Result<()> {
    run(libtorrent_downloads_what_rqbit_parsed(|l| {
        single_file("rq single ü.bin", 9 * PIECE + 77, l)
    }))
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_LIBTORRENT_PYTHON"]
async fn test_interop_libtorrent_downloads_multi_file_from_rqbit_url_list() -> anyhow::Result<()> {
    run(libtorrent_downloads_what_rqbit_parsed(|l| {
        nested("rq multi", l)
    }))
    .await
}
