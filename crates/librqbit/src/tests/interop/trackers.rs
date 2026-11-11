//! Trackers against libtorrent: the tiers of a torrent file it writes, and what it announces
//! compared with what rqbit announces for the same torrent state.

use std::{collections::BTreeSet, path::Path, time::Duration};

use anyhow::Context;
use serde_json::json;

use crate::{
    AddTorrent, AddTorrentOptions,
    api::TorrentIdOrHash,
    tests::{
        e2e_trackers::{FakeTracker, tracker_session},
        fake_peer::run,
    },
};

use super::Libtorrent;

const WAIT: Duration = Duration::from_secs(30);

// Writes a torrent file of the file argv[1]. Every other argument is "<tier>,<url>".
const CREATE_PY: &str = r#"
def main():
    path = os.path.abspath(sys.argv[1])
    fs = lt.file_storage()
    lt.add_files(fs, path)
    ct = lt.create_torrent(fs, piece_size=16384, flags=lt.create_torrent.v1_only)
    for arg in sys.argv[2:]:
        tier, url = arg.split(",", 1)
        ct.add_tracker(url, int(tier))
    lt.set_piece_hashes(ct, os.path.dirname(path))
    out = os.path.abspath("created.torrent")
    with open(out, "wb") as f:
        f.write(lt.bencode(ct.generate()))
    emit("created", path=out)

run(main)
"#;

// Seeds the torrent argv[1] from the folder argv[2], announcing to whatever the torrent file
// says. On "remove" it removes the torrent, which makes it announce "stopped".
const SEED_PY: &str = r#"
def main():
    session = make_session()
    info = lt.torrent_info(sys.argv[1])
    params = started_params(info, save_path_for(info, sys.argv[2]))
    params.flags |= lt.torrent_flags.seed_mode
    handle = session.add_torrent(params)
    emit("added")
    while True:
        pop_alerts(session, 0.1)
        cmd = next_command(timeout=0.1)
        if cmd and cmd["cmd"] == "remove":
            session.remove_torrent(handle)
        if cmd and cmd["cmd"] == "quit":
            return

run(main)
"#;

/// A file of a few pieces in a folder of its own, and the torrent libtorrent makes of it.
async fn create_torrent_with(
    dir: &Path,
    trackers: &[(usize, String)],
) -> anyhow::Result<(Libtorrent, std::path::PathBuf)> {
    let file = dir.join("content.bin");
    let content: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    tokio::fs::write(&file, content).await?;
    let mut args = vec![file.to_str().context("path")?.to_owned()];
    args.extend(trackers.iter().map(|(tier, url)| format!("{tier},{url}")));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut lt = Libtorrent::spawn(CREATE_PY, &args).await?;
    let created = lt.expect("created", WAIT).await?;
    let path = created
        .get("path")
        .and_then(|p| p.as_str())
        .context("no path")?
        .into();
    Ok((lt, path))
}

// libtorrent keeps the tiers of announce-list, and so does rqbit.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_LIBTORRENT_PYTHON"]
async fn test_interop_tiers_of_a_torrent_made_by_libtorrent() -> anyhow::Result<()> {
    run(async {
        let content_dir = tempfile::tempdir()?;
        let wanted: Vec<(usize, String)> = vec![
            (0, "http://127.0.0.1:1/a/announce".to_owned()),
            (0, "http://127.0.0.1:2/b/announce".to_owned()),
            (1, "udp://127.0.0.1:3/announce".to_owned()),
            (2, "http://127.0.0.1:4/c/announce".to_owned()),
        ];
        let (_lt, torrent) = create_torrent_with(content_dir.path(), &wanted).await?;

        let session_dir = tempfile::tempdir()?;
        let session = tracker_session(session_dir.path()).await?;
        // Paused: this is about what the torrent file says, not about reaching the trackers.
        let handle = session
            .add_torrent(
                AddTorrent::from_bytes(tokio::fs::read(&torrent).await?),
                Some(AddTorrentOptions {
                    paused: true,
                    overwrite: true,
                    output_folder: Some(content_dir.path().to_str().context("path")?.to_owned()),
                    ..Default::default()
                }),
            )
            .await?
            .into_handle()
            .context("into_handle")?;

        let rows = handle.trackers();
        for tier in 0..3 {
            let got: BTreeSet<&str> = rows
                .iter()
                .filter(|r| r.tier == tier)
                .map(|r| r.url.as_str())
                .collect();
            let expected: BTreeSet<&str> = wanted
                .iter()
                .filter(|(t, _)| *t == tier)
                .map(|(_, u)| u.as_str())
                .collect();
            assert_eq!(got, expected, "tier {tier} of {rows:?}");
        }
        assert_eq!(rows.len(), wanted.len(), "{rows:?}");
        Ok(())
    })
    .await
}

// The same complete torrent announced by rqbit and by libtorrent: same info hash, same events,
// same `left`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_LIBTORRENT_PYTHON"]
async fn test_interop_announces_match_libtorrent() -> anyhow::Result<()> {
    run(async {
        let tracker = FakeTracker::start().await?;
        let content_dir = tempfile::tempdir()?;
        let (_creator, torrent) =
            create_torrent_with(content_dir.path(), &[(0, tracker.url())]).await?;

        // rqbit: started, then stopped when the torrent is deleted.
        let session_dir = tempfile::tempdir()?;
        let session = tracker_session(session_dir.path()).await?;
        let handle = session
            .add_torrent(
                AddTorrent::from_bytes(tokio::fs::read(&torrent).await?),
                Some(AddTorrentOptions {
                    paused: false,
                    overwrite: true,
                    output_folder: Some(content_dir.path().to_str().context("path")?.to_owned()),
                    ..Default::default()
                }),
            )
            .await?
            .into_handle()
            .context("into_handle")?;
        handle.wait_until_completed().await?;
        tracker.wait_for_announces(1, WAIT).await?;
        session
            .delete(TorrentIdOrHash::Id(handle.id()), false)
            .await?;
        tracker.wait_for_announces(2, WAIT).await?;

        // libtorrent: the same.
        let mut lt = Libtorrent::spawn(
            SEED_PY,
            &[
                torrent.to_str().context("path")?,
                content_dir.path().to_str().context("path")?,
            ],
        )
        .await?;
        lt.expect("added", WAIT).await?;
        tracker.wait_for_announces(3, WAIT).await?;
        lt.send(&json!({"cmd": "remove"})).await?;
        tracker.wait_for_announces(4, WAIT).await?;

        let all = tracker.announces();
        let (ours, theirs) = all.split_at(2);
        for (a, b) in ours.iter().zip(theirs) {
            assert_eq!(a.event(), b.event(), "event");
            assert_eq!(a.left(), b.left(), "left");
            assert_eq!(a.param("info_hash"), b.param("info_hash"), "info_hash");
        }
        assert_eq!(theirs[0].event().as_deref(), Some("started"));
        assert_eq!(theirs[1].event().as_deref(), Some("stopped"));
        lt.send(&json!({"cmd": "quit"})).await?;
        Ok(())
    })
    .await
}
