//! Torrents made by `create_torrent` against the ones libtorrent makes: from the same content,
//! piece length and `info` options both give the same info hashes (so the same file order,
//! padding and fields in `info`); libtorrent reads every field of a torrent rqbit made, and
//! downloads with it.

use std::{
    net::Ipv4Addr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use serde_json::{Value, json};
use tokio::time::timeout;

use crate::{
    CreateTorrentOptions, ListenerOptions, Session, SessionOptions, TorrentVersion, create_torrent,
    create_torrent_file::tests::{
        LAYOUT_PIECE_LENGTH, SINGLE_FILE_LENGTHS, TestFile, data, folder_cases, write_files,
    },
    spawn_utils::BlockingSpawner,
    tests::fake_peer::run,
};

use super::{Libtorrent, port_of};

const WAIT: Duration = Duration::from_secs(30);
const FORMATS: [TorrentVersion; 3] = [
    TorrentVersion::V1,
    TorrentVersion::V2,
    TorrentVersion::Hybrid,
];

// For every command, describes the torrent file `cmd["torrent"]` that rqbit made, and the
// torrent libtorrent makes from the content at `cmd["path"]` with the same piece length,
// version, `private` and `source`.
const CREATE_PY: &str = r#"
def canonical_files(root):
    """The files under `root` sorted by path, comparing the UTF-8 bytes of each path
    component: the order of the files in a v2 file tree."""
    found = []
    for folder, _dirs, names in os.walk(root):
        for name in names:
            full = os.path.join(folder, name)
            parts = os.path.relpath(full, root).split(os.sep)
            found.append((parts, os.path.getsize(full)))
    found.sort(key=lambda f: [part.encode() for part in f[0]])
    return found


def libtorrent_torrent(cmd):
    path = os.path.abspath(cmd["path"])
    parent, name = os.path.split(path)
    fs = lt.file_storage()
    if cmd["version"] == "v1":
        # libtorrent keeps the files of a v1-only torrent in the order they were added, and
        # sorts them only with `canonical_files`, which also pads them. rqbit sorts v1 torrents
        # without padding them, so the equivalent is adding the files in the canonical order.
        flags = lt.create_torrent.v1_only
        if os.path.isfile(path):
            fs.add_file(name, os.path.getsize(path))
        else:
            for parts, size in canonical_files(path):
                fs.add_file("/".join([name] + parts), size)
    else:
        # v2 and hybrid torrents: libtorrent walks the folder, sorts and pads by itself.
        flags = lt.create_torrent.v2_only if cmd["version"] == "v2" else 0
        lt.add_files(fs, path)
    ct = lt.create_torrent(fs, cmd["piece_length"], flags)
    ct.set_priv(cmd["private"])
    lt.set_piece_hashes(ct, parent)
    entry = ct.generate()
    if cmd["source"]:
        # The Python bindings have no setter for `source`.
        entry[b"info"][b"source"] = cmd["source"].encode()
    return lt.torrent_info(lt.bencode(entry))


def describe(info):
    hashes = info.info_hashes()
    files = info.files()
    return {
        "v1": str(hashes.v1) if hashes.has_v1() else None,
        "v2": str(hashes.v2) if hashes.has_v2() else None,
        "name": info.name(),
        "piece_length": info.piece_length(),
        "num_pieces": info.num_pieces(),
        "files": [
            {
                "path": files.file_path(i),
                "size": files.file_size(i),
                "offset": files.file_offset(i),
                "pad": bool(files.file_flags(i) & lt.file_storage.flag_pad_file),
            }
            for i in range(files.num_files())
        ],
        "trackers": [[t.url, t.tier] for t in info.trackers()],
        "web_seeds": [w["url"] for w in info.web_seeds()],
        "private": info.priv(),
        "comment": info.comment(),
        "creator": info.creator(),
        "creation_date": info.creation_date(),
    }


def main():
    while True:
        cmd = next_command()
        if cmd is None or cmd["cmd"] == "quit":
            return
        rqbit = describe(lt.torrent_info(cmd["torrent"]))
        try:
            theirs = describe(libtorrent_torrent(cmd))
        except RuntimeError as e:
            theirs = {"error": str(e)}
        emit("created", rqbit=rqbit, libtorrent=theirs)

run(main)
"#;

// Checks the content in the folder argv[2] against the torrent argv[1], then seeds it.
const CHECK_AND_SEED_PY: &str = r#"
def main():
    session = make_session()
    info = lt.torrent_info(sys.argv[1])
    handle = session.add_torrent(started_params(info, save_path_for(info, sys.argv[2])))
    while not any(isinstance(a, lt.torrent_checked_alert) for a in pop_alerts(session)):
        pass
    status = handle.status()
    emit(
        "checked",
        seeding=status.is_seeding,
        pieces=status.num_pieces,
        total=info.num_pieces(),
    )
    emit("listening", port=wait_listen_port(session))
    while True:
        cmd = next_command(timeout=0.2)
        if cmd and cmd["cmd"] == "quit":
            return

run(main)
"#;

// Downloads the torrent argv[1] from the peer at 127.0.0.1:argv[2] into the folder argv[3].
const LEECH_PY: &str = r#"
def main():
    session = make_session()
    handle = session.add_torrent(started_params(lt.torrent_info(sys.argv[1]), sys.argv[3]))
    handle.connect_peer(("127.0.0.1", int(sys.argv[2])))
    while True:
        cmd = next_command(timeout=0.2)
        if cmd and cmd["cmd"] == "quit":
            return
        if handle.status().is_seeding:
            emit("finished")
            return

run(main)
"#;

/// Whether libtorrent fails to make a hybrid torrent of the case. It orders the v1 file list
/// of a hybrid torrent by whole path strings, but its v2 file tree component by component:
/// `a/x` comes before `a.b` in the tree, after it as a string ('.' < '/'). It then refuses to
/// load the torrent it made, whose two file lists disagree. rqbit orders both by component.
fn libtorrent_cannot_make_hybrid(case: &Case) -> bool {
    let paths = case.files.iter().map(|(p, _)| p.as_str());
    let mut by_string: Vec<&str> = paths.clone().collect();
    by_string.sort_unstable();
    !by_string.into_iter().eq(paths)
}

fn version_name(version: TorrentVersion) -> &'static str {
    match version {
        TorrentVersion::V1 => "v1",
        TorrentVersion::V2 => "v2",
        TorrentVersion::Hybrid => "hybrid",
    }
}

/// A layout test case: the content (a file or a folder) and its files, in torrent order,
/// with their paths in the torrent (starting with the torrent's name).
struct Case {
    name: String,
    _dir: tempfile::TempDir,
    path: PathBuf,
    files: Vec<TestFile>,
}

/// The layouts of the unit tests: folders named `content`, and single files.
fn cases() -> anyhow::Result<Vec<Case>> {
    let mut out = Vec::new();
    for (name, files) in folder_cases() {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("content");
        write_files(&path, &files);
        let files = files
            .into_iter()
            .map(|(p, content)| (format!("content/{p}"), content))
            .collect();
        out.push(Case {
            name: name.to_owned(),
            _dir: dir,
            path,
            files,
        });
    }
    for len in SINGLE_FILE_LENGTHS {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("single.bin");
        let content = data(len as u64, len);
        std::fs::write(&path, &content)?;
        out.push(Case {
            name: format!("a single file of {len} bytes"),
            _dir: dir,
            path,
            files: vec![("single.bin".to_owned(), content)],
        });
    }
    Ok(out)
}

const TRACKER_TIERS: [&[&str]; 2] = [
    &[
        "http://t1.example/announce",
        "udp://t2.example:6969/announce",
    ],
    &["http://t3.example/announce"],
];
const WEB_SEEDS: [&str; 2] = ["http://ws1.example/files/", "https://ws2.example/x"];
const SOURCE: &str = "interop-source";
const COMMENT: &str = "a comment, ünïcødé";
const CREATED_BY: &str = "an interop test";
const CREATION_DATE: i64 = 1_700_000_000;

/// Without `full`, the defaults; with it, every field a torrent can have.
fn options(version: TorrentVersion, full: bool) -> CreateTorrentOptions<'static> {
    let mut options = CreateTorrentOptions {
        piece_length: Some(LAYOUT_PIECE_LENGTH),
        version,
        ..Default::default()
    };
    if full {
        options.tracker_tiers = TRACKER_TIERS
            .iter()
            .map(|tier| tier.iter().map(|t| t.to_string()).collect())
            .collect();
        options.web_seeds = WEB_SEEDS.iter().map(|w| w.to_string()).collect();
        options.private = true;
        options.source = Some(SOURCE);
        options.comment = Some(COMMENT);
        options.created_by = Some(CREATED_BY);
        options.creation_date = Some(CREATION_DATE);
    }
    options
}

/// What libtorrent read from the torrent rqbit made matches the options and the content.
fn check_read_back(case: &Case, full: bool, rqbit: &Value) {
    let ctx = format!("{}, full options: {full}", case.name);
    let files: Vec<(&str, u64)> = rqbit["files"]
        .as_array()
        .unwrap_or_else(|| panic!("{ctx}: no files in {rqbit}"))
        .iter()
        .filter(|f| f["pad"] == json!(false))
        .map(|f| {
            (
                f["path"].as_str().unwrap_or_default(),
                f["size"].as_u64().unwrap_or(u64::MAX),
            )
        })
        .collect();
    let expected: Vec<(&str, u64)> = case
        .files
        .iter()
        .map(|(p, content)| (p.as_str(), content.len() as u64))
        .collect();
    assert_eq!(files, expected, "{ctx}");
    let trackers = if full {
        json!([
            [TRACKER_TIERS[0][0], 0],
            [TRACKER_TIERS[0][1], 0],
            [TRACKER_TIERS[1][0], 1],
        ])
    } else {
        json!([])
    };
    assert_eq!(rqbit["trackers"], trackers, "{ctx}");
    // libtorrent appends a '/' to the web seeds it takes for folders (which depends on the
    // torrent's format), so a trailing '/' is not compared.
    let web_seeds: Vec<&str> = rqbit["web_seeds"]
        .as_array()
        .unwrap_or_else(|| panic!("{ctx}: no web seeds in {rqbit}"))
        .iter()
        .map(|w| w.as_str().unwrap_or_default().trim_end_matches('/'))
        .collect();
    let expected: Vec<&str> = WEB_SEEDS
        .iter()
        .filter(|_| full)
        .map(|w| w.trim_end_matches('/'))
        .collect();
    assert_eq!(web_seeds, expected, "{ctx}");
    assert_eq!(rqbit["private"], json!(full), "{ctx}");
    assert_eq!(
        rqbit["comment"],
        json!(if full { COMMENT } else { "" }),
        "{ctx}"
    );
    let creator = if full {
        CREATED_BY
    } else {
        crate::client_name_and_version()
    };
    assert_eq!(rqbit["creator"], json!(creator), "{ctx}");
    let date = rqbit["creation_date"].as_i64().unwrap_or(0);
    assert_eq!(date, if full { CREATION_DATE } else { 0 }, "{ctx}");
}

// The decisive test of the file order, the padding and the `info` fields: libtorrent makes the
// same torrent from the same content, in every format, over every layout.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_LIBTORRENT_PYTHON"]
async fn test_interop_create_torrent_same_info_hashes_as_libtorrent() -> anyhow::Result<()> {
    run(async {
        let mut lt = Libtorrent::spawn(CREATE_PY, &[]).await?;
        let spawner = BlockingSpawner::new(1);
        for case in cases()? {
            for version in FORMATS {
                for full in [false, true] {
                    let ctx = format!("{} ({version:?}, full options: {full})", case.name);
                    let torrent = create_torrent(&case.path, options(version, full), &spawner)
                        .await
                        .with_context(|| ctx.clone())?;
                    let file = case._dir.path().join("rqbit.torrent");
                    tokio::fs::write(&file, torrent.as_bytes()?).await?;
                    lt.send(&json!({
                        "cmd": "create",
                        "torrent": file.to_str().context("path")?,
                        "path": case.path.to_str().context("path")?,
                        "version": version_name(version),
                        "piece_length": LAYOUT_PIECE_LENGTH,
                        "private": full,
                        "source": if full { Some(SOURCE) } else { None },
                    }))
                    .await?;
                    let created = lt
                        .expect("created", WAIT)
                        .await
                        .with_context(|| ctx.clone())?;
                    let (rqbit, theirs) = (&created["rqbit"], &created["libtorrent"]);

                    // libtorrent sees the info hashes rqbit computed...
                    let v1 = version.has_v1().then(|| torrent.info_hash().as_string());
                    assert_eq!(rqbit["v1"], json!(v1), "{ctx}");
                    let v2 = torrent.info_hash_v2().map(|h| h.as_string());
                    assert_eq!(rqbit["v2"], json!(v2), "{ctx}");
                    assert_eq!(v2.is_some(), version.has_v2(), "{ctx}");
                    if version == TorrentVersion::Hybrid && libtorrent_cannot_make_hybrid(&case) {
                        let error = theirs["error"].as_str().unwrap_or_default();
                        assert!(
                            error.contains("the v1 and v2 file metadata does not match"),
                            "{ctx}: libtorrent made a hybrid torrent of it: {theirs}"
                        );
                    } else {
                        // ...and makes the same torrent. The layouts first, for a readable
                        // failure.
                        for field in ["name", "piece_length", "num_pieces", "files"] {
                            assert_eq!(rqbit[field], theirs[field], "{ctx}: {field}");
                        }
                        assert_eq!(rqbit["v1"], theirs["v1"], "{ctx}: v1 info hash");
                        assert_eq!(rqbit["v2"], theirs["v2"], "{ctx}: v2 info hash");
                    }

                    check_read_back(&case, full, rqbit);
                }
            }
        }
        lt.send(&json!({"cmd": "quit"})).await?;
        Ok(())
    })
    .await
}

/// A session that seeds on loopback.
async fn rqbit_session(dir: &Path) -> anyhow::Result<Arc<Session>> {
    Session::new_with_opts(
        dir.to_owned(),
        SessionOptions {
            dht: None,
            persistence: None,
            listen: Some(ListenerOptions {
                listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
                ipv4_only: true,
                ..Default::default()
            }),
            ipv4_only: true,
            disable_local_service_discovery: true,
            ..Default::default()
        },
    )
    .await
    .context("error creating session")
}

/// Whether rqbit seeds a torrent of this format. The v2 download and seeding paths of the
/// session are not on this branch, so only v1 torrents are seeded by rqbit here.
fn rqbit_seeds(version: TorrentVersion) -> bool {
    version == TorrentVersion::V1
}

// libtorrent checks a folder against the torrent rqbit made of it (every piece hash, v1 and
// v2, must be right for it to become a seeder), then another libtorrent downloads the folder
// with that torrent, in every format. rqbit seeds the v1 torrent with
// `create_and_serve_torrent`; the v2 and hybrid ones are seeded by the libtorrent that checked
// them.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_LIBTORRENT_PYTHON"]
async fn test_interop_libtorrent_downloads_created_torrents() -> anyhow::Result<()> {
    run(async {
        let (_, files) = folder_cases()
            .into_iter()
            .next()
            .context("no folder case")?;
        for version in FORMATS {
            let dir = tempfile::tempdir()?;
            let content = dir.path().join("content");
            write_files(&content, &files);
            let options = CreateTorrentOptions {
                piece_length: Some(LAYOUT_PIECE_LENGTH),
                version,
                ..Default::default()
            };
            let (torrent, session) = if rqbit_seeds(version) {
                let session = rqbit_session(&dir.path().join("session")).await?;
                let (torrent, handle) = session.create_and_serve_torrent(&content, options).await?;
                timeout(WAIT, handle.wait_until_completed())
                    .await
                    .context("the rqbit seeder did not become a seeder")??;
                (torrent, Some(session))
            } else {
                let torrent = create_torrent(&content, options, &BlockingSpawner::new(1)).await?;
                (torrent, None)
            };
            let file = dir.path().join("rqbit.torrent");
            tokio::fs::write(&file, torrent.as_bytes()?).await?;
            let file = file.to_str().context("path")?;

            let mut checker = Libtorrent::spawn(
                CHECK_AND_SEED_PY,
                &[file, content.to_str().context("path")?],
            )
            .await?;
            let checked = checker.expect("checked", WAIT).await?;
            assert_eq!(
                checked["seeding"],
                json!(true),
                "{version:?}: libtorrent's check failed: {checked}"
            );
            let port = match &session {
                Some(session) => session.listen_addr().context("no listen addr")?.port(),
                None => port_of(&checker.expect("listening", WAIT).await?, "port")?,
            };

            let out = dir.path().join("leech");
            tokio::fs::create_dir_all(&out).await?;
            let mut leech = Libtorrent::spawn(
                LEECH_PY,
                &[file, &port.to_string(), out.to_str().context("path")?],
            )
            .await?;
            leech.expect("finished", WAIT).await?;
            for (path, expected) in &files {
                let downloaded = tokio::fs::read(out.join("content").join(path)).await?;
                assert!(downloaded == *expected, "{version:?}: {path} differs");
            }

            checker.send(&json!({"cmd": "quit"})).await?;
            if let Some(session) = session {
                session.stop().await;
            }
        }
        Ok(())
    })
    .await
}
