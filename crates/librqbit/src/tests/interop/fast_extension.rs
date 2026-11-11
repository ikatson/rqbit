//! The Fast Extension (BEP 6) against libtorrent: both sides negotiate it, the piece sets go
//! out as HaveAll / HaveNone, allowed-fast sets are offered, and a choke with requests in
//! flight is answered with rejects that rqbit recovers from.
//!
//! What libtorrent sent and received is proven from its peer log, which has one line per
//! message, such as `<== HAVE_ALL` (received) or `==> REJECT` (sent; a received reject is
//! `REJECT_PIECE`).

use std::{net::SocketAddr, time::Duration};

use anyhow::Context;
use serde_json::{Value, json};
use tokio::time::timeout;

use crate::tests::{
    fake_peer::{
        ClientOpts, Fixture, SeederExt, SeederOpts, run, start_client, start_seeder,
        start_seeder_ext,
    },
    test_util::wait_until,
};

use super::{Libtorrent, port_of};

// Below the 60 seconds of `run`, so that a step that hangs says which one it is.
const WAIT: Duration = Duration::from_secs(20);

// The peer log lines of the Fast Extension messages and of chokes, in both directions. Reported
// to the test with the "peer_log" event.
const PEER_LOG_PY: &str = r#"
FAST_MESSAGES = ("HAVE_ALL", "HAVE_NONE", "ALLOWED_FAST", "REJECT", "REJECT_PIECE", "SUGGEST_PIECE", "CHOKE", "UNCHOKE")

def collect_peer_log(session, lines, wait=0.2):
    for a in pop_alerts(session, wait):
        if isinstance(a, lt.peer_log_alert):
            message = a.message()
            for direction in ("<==", "==>"):
                for name in FAST_MESSAGES:
                    if f"{direction} {name} " in message or message.endswith(f"{direction} {name}"):
                        lines.append(f"{direction} {name}")
"#;

// Seeds the content in the folder argv[2], described by the torrent file argv[1], with the
// session settings in the JSON object argv[3]. Commands: "settings" applies more settings,
// "quit" reports the peer log and the bytes uploaded.
const SEED_PY: &str = r#"
def main():
    session = make_session(**json.loads(sys.argv[3]))
    # Loopback peers fall in the local peer class, which the rate limits do not apply to. Put
    # every peer in the global class only, so that they do.
    classes = lt.ip_filter()
    classes.add_rule("0.0.0.0", "255.255.255.255", 1 << session.global_peer_class_id)
    session.set_peer_class_filter(classes)
    info = lt.torrent_info(sys.argv[1])
    params = started_params(info, save_path_for(info, sys.argv[2]))
    params.flags |= lt.torrent_flags.seed_mode
    handle = session.add_torrent(params)
    emit("listening", port=wait_listen_port(session))
    lines = []
    while True:
        # Every line once, in order: a choke the test waits for may follow an earlier one.
        reported = len(lines)
        collect_peer_log(session, lines)
        for line in lines[reported:]:
            emit("seen", line=line)
        cmd = next_command(timeout=0.01)
        if cmd is None:
            continue
        if cmd["cmd"] == "settings":
            session.apply_settings(cmd["settings"])
            emit("applied")
        elif cmd["cmd"] == "quit":
            collect_peer_log(session, lines, wait=0)
            emit("peer_log", lines=lines, uploaded=handle.status().total_payload_upload)
            return

run(main)
"#;

// Downloads the torrent argv[1] from the peer at 127.0.0.1:argv[2] into the current folder,
// then reports the peer log.
const LEECH_PY: &str = r#"
def main():
    session = make_session()
    params = started_params(lt.torrent_info(sys.argv[1]), ".")
    handle = session.add_torrent(params)
    handle.connect_peer(("127.0.0.1", int(sys.argv[2])))
    lines = []
    while True:
        collect_peer_log(session, lines)
        cmd = next_command(timeout=0.01)
        if cmd and cmd["cmd"] == "quit":
            return
        status = handle.status()
        if status.is_seeding:
            collect_peer_log(session, lines, wait=0)
            emit(
                "peer_log",
                lines=lines,
                downloaded=status.total_payload_download,
                file=first_file_path(params.ti),
            )
            return

run(main)
"#;

fn script(body: &str) -> String {
    format!("{PEER_LOG_PY}\n{body}")
}

fn peer_log(event: &Value) -> anyhow::Result<Vec<String>> {
    event
        .get("lines")
        .and_then(Value::as_array)
        .context("no lines in the peer log event")?
        .iter()
        .map(|line| line.as_str().map(str::to_owned).context("not a string"))
        .collect()
}

fn saw(log: &[String], line: &str) -> bool {
    log.iter().any(|l| l == line)
}

/// Starts libtorrent seeding the content of `seed` with `settings`, and returns it with the
/// address it listens on.
async fn libtorrent_seeder(
    seed: &Fixture,
    settings: Value,
) -> anyhow::Result<(Libtorrent, SocketAddr, tempfile::TempDir)> {
    seed.session.stop().await;
    let torrent_dir = tempfile::tempdir()?;
    let torrent = torrent_dir.path().join("interop.torrent");
    tokio::fs::write(&torrent, &seed.torrent_bytes).await?;
    let mut lt = Libtorrent::spawn(
        &script(SEED_PY),
        &[
            torrent.to_str().context("path")?,
            seed._dir.path().to_str().context("path")?,
            &settings.to_string(),
        ],
    )
    .await?;
    let listening = lt.expect("listening", WAIT).await?;
    let addr = SocketAddr::from(([127, 0, 0, 1], port_of(&listening, "port")?));
    Ok((lt, addr, torrent_dir))
}

// rqbit downloads from a libtorrent seed: libtorrent announces its pieces with HaveAll and
// hears HaveNone from rqbit, which proves that both handshakes carried the bit.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_LIBTORRENT_PYTHON"]
async fn test_interop_fast_download_from_libtorrent() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let (mut lt, addr, _torrent_dir) = libtorrent_seeder(&seed, json!({})).await?;
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
        let log = peer_log(&lt.expect("peer_log", WAIT).await?)?;
        assert!(
            saw(&log, "==> HAVE_ALL"),
            "libtorrent sent no HaveAll: {log:?}"
        );
        assert!(
            saw(&log, "<== HAVE_NONE"),
            "rqbit sent no HaveNone: {log:?}"
        );
        Ok(())
    })
    .await
}

// libtorrent downloads from an rqbit seed: rqbit announces its pieces with HaveAll, hears
// HaveNone, and offers the newcomer its allowed-fast set.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_LIBTORRENT_PYTHON"]
async fn test_interop_fast_upload_to_libtorrent() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let torrent_dir = tempfile::tempdir()?;
        let torrent = torrent_dir.path().join("interop.torrent");
        tokio::fs::write(&torrent, &seed.torrent_bytes).await?;

        let port = seed.addr().port().to_string();
        let mut lt = Libtorrent::spawn(
            &script(LEECH_PY),
            &[torrent.to_str().context("path")?, &port],
        )
        .await?;
        let finished = lt.expect("peer_log", WAIT).await?;
        assert_eq!(
            finished.get("downloaded").and_then(Value::as_u64),
            Some(seed.content.len() as u64)
        );
        let file = finished
            .get("file")
            .and_then(Value::as_str)
            .context("no file in the event")?;
        let downloaded = tokio::fs::read(file).await?;
        assert!(downloaded == seed.content, "content differs");

        let log = peer_log(&finished)?;
        assert!(saw(&log, "<== HAVE_ALL"), "rqbit sent no HaveAll: {log:?}");
        assert!(
            saw(&log, "==> HAVE_NONE"),
            "libtorrent sent no HaveNone: {log:?}"
        );
        assert!(
            saw(&log, "<== ALLOWED_FAST"),
            "rqbit offered no allowed-fast piece: {log:?}"
        );
        Ok(())
    })
    .await
}

// libtorrent chokes rqbit in the middle of a download, with requests in flight: it rejects
// them (BEP 6), rqbit gives them back, and the download completes once libtorrent unchokes
// again.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_LIBTORRENT_PYTHON"]
async fn test_interop_fast_libtorrent_chokes_mid_download() -> anyhow::Result<()> {
    run(async {
        // 1 MiB at 128 KiB/s: slow enough to choke in the middle, with requests queued at
        // libtorrent. Unchoke rounds every second instead of every 15, so that the change of
        // the slot count takes effect right away.
        let seed = start_seeder_ext(
            SeederOpts::default(),
            SeederExt {
                pieces: Some(32),
                ..Default::default()
            },
        )
        .await?;
        let total = seed.content.len() as u64;
        let (mut lt, addr, _torrent_dir) = libtorrent_seeder(
            &seed,
            json!({"upload_rate_limit": 128 * 1024, "unchoke_interval": 1}),
        )
        .await?;
        // A read timeout far longer than the test: the safety net for a peer that chokes and
        // answers nothing (half of it) cannot be what gives the requests back.
        let client = start_client(
            &seed,
            ClientOpts {
                initial_peers: vec![addr],
                read_write_timeout: Some(Duration::from_secs(60)),
                ..Default::default()
            },
        )
        .await?;
        let progress = || client.handle.stats().progress_bytes;
        wait_until(
            || {
                anyhow::ensure!(progress() >= total / 8, "progress {}", progress());
                Ok(())
            },
            WAIT,
        )
        .await?;
        tracing::info!(progress = progress(), "choking rqbit");

        // No upload slot at all: libtorrent chokes rqbit at its next unchoke round.
        lt.send(&json!({"cmd": "settings", "settings": {"unchoke_slots_limit": 0}}))
            .await?;
        lt.expect("applied", WAIT).await?;
        loop {
            let seen = lt.expect("seen", WAIT).await?;
            tracing::debug!(%seen, progress = progress(), "libtorrent peer log");
            if seen.get("line").and_then(Value::as_str) == Some("==> CHOKE") {
                break;
            }
        }
        let at_choke = progress();
        assert!(at_choke < total, "finished before the choke");
        // libtorrent answers what rqbit asked for with rejects (or the blocks it was already
        // sending), and rqbit forgets each answered request: nothing stays in flight while
        // libtorrent keeps rqbit choked.
        wait_until(
            || {
                let live = client.handle.live().context("not live")?;
                let inflight = live.inflight_requests_to(addr);
                anyhow::ensure!(inflight == Some(0), "in flight to libtorrent: {inflight:?}");
                Ok(())
            },
            WAIT,
        )
        .await?;
        assert!(progress() < total, "finished while choked");

        lt.send(&json!({"cmd": "settings", "settings": {"unchoke_slots_limit": 8}}))
            .await?;
        timeout(WAIT, client.handle.wait_until_completed())
            .await
            .context("the download did not complete after the unchoke")??;
        let downloaded = tokio::fs::read(client._dir.path().join("0.data")).await?;
        assert!(downloaded == seed.content, "content differs");

        lt.send(&json!({"cmd": "quit"})).await?;
        let log = peer_log(&lt.expect("peer_log", WAIT).await?)?;
        assert!(saw(&log, "==> CHOKE"), "libtorrent did not choke: {log:?}");
        assert!(
            saw(&log, "==> REJECT"),
            "libtorrent rejected nothing at the choke: {log:?}"
        );
        assert!(
            saw(&log, "==> UNCHOKE"),
            "libtorrent did not unchoke: {log:?}"
        );
        Ok(())
    })
    .await
}
