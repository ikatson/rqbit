//! Super seeding (BEP 16) against libtorrent, both ways: libtorrent leechers download from an
//! rqbit super seeder, and rqbit leechers download from a libtorrent super seeder.

use std::{collections::BTreeMap, net::SocketAddr, time::Duration};

use anyhow::{Context, ensure};
use serde_json::{Value, json};
use tokio::time::timeout;

use crate::tests::fake_peer::{
    ClientOpts, Fixture, PIECE_LEN, SeederExt, SeederOpts, run, start_client, start_seeder_ext,
    uploaded,
};

use super::{Libtorrent, port_of};

const WAIT: Duration = Duration::from_secs(60);

/// Enough pieces that a leecher waiting for something between them would be seen.
const PIECES: u32 = 32;

// Downloads the torrent argv[1] into the current folder. Reports its port as "listening",
// connects to 127.0.0.1:<port> on {"cmd": "connect", "port": <port>}, reports "finished" once
// it has everything, and keeps seeding to its peers until "quit".
const LEECH_PY: &str = r#"
def main():
    session = make_session()
    params = started_params(lt.torrent_info(sys.argv[1]), ".")
    handle = session.add_torrent(params)
    emit("listening", port=wait_listen_port(session))
    finished = False
    while True:
        cmd = next_command(timeout=0.1)
        if cmd and cmd["cmd"] == "quit":
            return
        if cmd and cmd["cmd"] == "connect":
            handle.connect_peer(("127.0.0.1", cmd["port"]))
        if not finished and handle.status().is_seeding:
            finished = True
            emit("finished", file=first_file_path(params.ti))

run(main)
"#;

// Checks the content in the folder argv[2], described by the torrent file argv[1], turns super
// seeding on once it is a seed, and then reports its port as "listening". Records the name of
// every message it sends (the python binding of peer_log_alert has no direction, so it is read
// from the message, which libtorrent formats as "... ==> HAVE_NONE" for an outgoing one). On
// "quit" it reports how many of each it sent, its upload and whether it still super seeds.
const SUPER_SEED_PY: &str = r#"
import re

OUTGOING = re.compile(r" ==> ([A-Z_]+)")

def collect(session, sent, wait):
    for a in pop_alerts(session, wait):
        if isinstance(a, lt.peer_log_alert):
            m = OUTGOING.search(a.message())
            if m is not None:
                sent[m.group(1)] = sent.get(m.group(1), 0) + 1

def main():
    session = make_session()
    info = lt.torrent_info(sys.argv[1])
    handle = session.add_torrent(started_params(info, save_path_for(info, sys.argv[2])))
    sent = {}
    while not handle.status().is_seeding:
        collect(session, sent, 0.1)
    handle.set_flags(lt.torrent_flags.super_seeding)
    if not handle.flags() & lt.torrent_flags.super_seeding:
        raise RuntimeError("libtorrent did not turn super seeding on")
    emit("listening", port=wait_listen_port(session), version=lt.version)
    while True:
        collect(session, sent, 0.1)
        cmd = next_command(timeout=0)
        if cmd and cmd["cmd"] == "quit":
            collect(session, sent, 0.2)
            emit(
                "done",
                sent=sent,
                uploaded=handle.status().total_payload_upload,
                super_seeding=bool(handle.flags() & lt.torrent_flags.super_seeding),
            )
            return

run(main)
"#;

/// A torrent of [`PIECES`] pieces. The session of the fixture seeds it when `super_seeding`,
/// and is stopped otherwise, leaving only its files.
async fn fixture(super_seeding: bool) -> anyhow::Result<Fixture> {
    let seed = start_seeder_ext(
        SeederOpts {
            // Slow enough that the leechers are better off trading with each other.
            upload_bps: super_seeding.then_some(256 * 1024),
            ..Default::default()
        },
        SeederExt {
            pieces: Some(PIECES),
            super_seeding,
            ..Default::default()
        },
    )
    .await?;
    if !super_seeding {
        seed.session.stop().await;
    }
    Ok(seed)
}

/// Writes the torrent file of `seed` to a new folder, returning both.
fn torrent_file(seed: &Fixture) -> anyhow::Result<(tempfile::TempDir, String)> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("super_seeding.torrent");
    std::fs::write(&path, &seed.torrent_bytes)?;
    let path = path.to_str().context("path")?.to_owned();
    Ok((dir, path))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_LIBTORRENT_PYTHON"]
async fn test_interop_libtorrent_leechers_download_from_rqbit_super_seeder() -> anyhow::Result<()> {
    run(async {
        let seed = fixture(true).await?;
        let (_torrent_dir, torrent) = torrent_file(&seed)?;
        let seed_port = seed.addr().port();

        let mut a = Libtorrent::spawn(LEECH_PY, &[&torrent]).await?;
        let a_port = port_of(&a.expect("listening", WAIT).await?, "port")?;
        let mut b = Libtorrent::spawn(LEECH_PY, &[&torrent]).await?;
        b.expect("listening", WAIT).await?;
        a.send(&json!({"cmd": "connect", "port": seed_port}))
            .await?;
        b.send(&json!({"cmd": "connect", "port": seed_port}))
            .await?;
        b.send(&json!({"cmd": "connect", "port": a_port})).await?;

        for (name, lt) in [("a", &mut a), ("b", &mut b)] {
            let finished = lt
                .expect("finished", WAIT)
                .await
                .with_context(|| format!("libtorrent leecher {name} did not finish"))?;
            let file = finished
                .get("file")
                .and_then(Value::as_str)
                .context("no file in the event")?;
            let downloaded = tokio::fs::read(file).await?;
            ensure!(
                downloaded == seed.content,
                "leecher {name} downloaded other content"
            );
        }
        let total = u64::from(PIECES * PIECE_LEN);
        let up = uploaded(&seed);
        ensure!(up >= total, "the seeder uploaded {up} bytes of {total}");
        ensure!(
            up < total * 3 / 2,
            "the super seeder uploaded {up} bytes for a torrent of {total}"
        );
        for lt in [&mut a, &mut b] {
            lt.send(&json!({"cmd": "quit"})).await?;
        }
        seed.session.stop().await;
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_LIBTORRENT_PYTHON"]
async fn test_interop_rqbit_leechers_download_from_libtorrent_super_seeder() -> anyhow::Result<()> {
    run(async {
        let seed = fixture(false).await?;
        let (_torrent_dir, torrent) = torrent_file(&seed)?;
        let content_dir = seed._dir.path().to_str().context("path")?;

        let mut lt = Libtorrent::spawn(SUPER_SEED_PY, &[&torrent, content_dir]).await?;
        let listening = lt.expect("listening", WAIT).await?;
        let lt_addr = SocketAddr::from(([127, 0, 0, 1], port_of(&listening, "port")?));

        let a = start_client(
            &seed,
            ClientOpts {
                listen: true,
                initial_peers: vec![lt_addr],
                ..Default::default()
            },
        )
        .await?;
        let b = start_client(
            &seed,
            ClientOpts {
                listen: true,
                initial_peers: vec![lt_addr, a.addr()],
                ..Default::default()
            },
        )
        .await?;
        // A leecher that waits for a bitfield, or announces its pieces late, makes the super
        // seeder wait for it on every piece; this bound is far below that.
        for (name, c) in [("a", &a), ("b", &b)] {
            timeout(WAIT, c.handle.wait_until_completed())
                .await
                .with_context(|| format!("rqbit leecher {name} did not complete"))??;
            let downloaded = tokio::fs::read(c._dir.path().join("0.data")).await?;
            ensure!(
                downloaded == seed.content,
                "leecher {name} downloaded other content"
            );
        }

        lt.send(&json!({"cmd": "quit"})).await?;
        let done = lt.expect("done", WAIT).await?;
        ensure!(
            done.get("super_seeding").and_then(Value::as_bool) == Some(true),
            "libtorrent stopped super seeding: {done}"
        );
        let sent: BTreeMap<String, u64> = serde_json::from_value(
            done.get("sent")
                .cloned()
                .context("no sent messages in the event")?,
        )?;
        // It showed the pieces one at a time, as a super seeder does.
        ensure!(
            !sent.contains_key("HAVE_ALL") && !sent.contains_key("BITFIELD"),
            "libtorrent advertised its pieces: {sent:?}"
        );
        ensure!(
            sent.get("HAVE").copied().unwrap_or_default() >= u64::from(PIECES),
            "libtorrent did not show every piece with a HAVE: {sent:?}"
        );
        for c in [&a, &b] {
            c.session.stop().await;
        }
        Ok(())
    })
    .await
}
