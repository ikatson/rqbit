//! Download priorities against libtorrent. Priorities are local, nothing about them goes on the
//! wire, so this is a sanity transfer: rqbit downloads a torrent with a skipped file and a
//! maximum one from a libtorrent seeder, and libtorrent's own log of the requests it received
//! shows which pieces rqbit asked for.

use std::{collections::BTreeSet, net::SocketAddr, time::Duration};

use anyhow::Context;
use serde_json::{Value, json};
use tokio::time::timeout;

use crate::{
    AddTorrent, AddTorrentOptions, Session,
    tests::{
        e2e_priorities::{NAME, PIECE, client_session_opts, make_torrent},
        fake_peer::run,
    },
};

use super::{Libtorrent, port_of};

const WAIT: Duration = Duration::from_secs(30);

// Seeds the content in the folder argv[2], described by the torrent file argv[1], and records
// the piece of every REQUEST message it receives, in order. The python binding of
// peer_log_alert has no direction or event type, so they are read from its message, which
// libtorrent formats as "<torrent> peer <ip:port> <== REQUEST [ piece: 6 s: 0 l: 4000 ]" for an
// incoming request. On "quit" it reports the pieces as the "requests" event.
const SEED_PY: &str = r#"
import re

REQUEST = re.compile(r" <== REQUEST \[ piece: (\d+) ")

def collect(session, pieces, wait):
    for a in pop_alerts(session, wait):
        if isinstance(a, lt.peer_log_alert):
            m = REQUEST.search(a.message())
            if m is not None:
                pieces.append(int(m.group(1)))

def main():
    session = make_session()
    info = lt.torrent_info(sys.argv[1])
    params = started_params(info, save_path_for(info, sys.argv[2]))
    params.flags |= lt.torrent_flags.seed_mode
    session.add_torrent(params)
    emit("listening", port=wait_listen_port(session), version=lt.version)
    pieces = []
    while True:
        collect(session, pieces, 0.1)
        cmd = next_command(timeout=0)
        if cmd and cmd["cmd"] == "quit":
            collect(session, pieces, 0.2)
            emit("requests", pieces=pieces)
            return

run(main)
"#;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_LIBTORRENT_PYTHON"]
async fn test_interop_priorities_download_from_libtorrent() -> anyhow::Result<()> {
    run(async {
        let p = PIECE as usize;
        // "a" is pieces 0..=2, "b" pieces 2..=5 and "c" pieces 5 and 6: pieces 3 and 4 belong
        // to "b" alone, 2 and 5 are shared with a wanted file.
        let torrent = make_torrent(&[2 * p + p / 2, 3 * p, p + p / 2]);

        // libtorrent keeps a multi-file torrent in a folder named after it.
        let seed_dir = tempfile::tempdir()?;
        let content_dir = seed_dir.path().join(NAME);
        std::fs::create_dir(&content_dir)?;
        for (name, offset, len) in &torrent.files {
            std::fs::write(
                content_dir.join(name),
                &torrent.content[*offset..offset + len],
            )?;
        }
        let torrent_file = seed_dir.path().join("prio.torrent");
        std::fs::write(&torrent_file, &torrent.bytes)?;

        let mut lt = Libtorrent::spawn(
            SEED_PY,
            &[
                torrent_file.to_str().context("path")?,
                content_dir.to_str().context("path")?,
            ],
        )
        .await?;
        let listening = lt.expect("listening", WAIT).await?;
        let addr = SocketAddr::from(([127, 0, 0, 1], port_of(&listening, "port")?));

        let client_dir = tempfile::tempdir()?;
        let session = Session::new_with_opts(client_dir.path().into(), client_session_opts(None))
            .await
            .context("error creating client session")?;
        let handle = session
            .add_torrent(
                AddTorrent::from_bytes(torrent.bytes.clone()),
                Some(AddTorrentOptions {
                    output_folder: Some(
                        client_dir
                            .path()
                            .to_str()
                            .context("temp dir path")?
                            .to_owned(),
                    ),
                    overwrite: true,
                    initial_peers: Some(vec![addr]),
                    file_priorities: Some(vec![1, 0, 7]),
                    ..Default::default()
                }),
            )
            .await?
            .into_handle()
            .context("into_handle")?;
        timeout(WAIT, handle.wait_until_completed())
            .await
            .context("the download from libtorrent timed out")??;

        for (name, offset, len) in &torrent.files {
            if name == "b" {
                continue;
            }
            let downloaded = tokio::fs::read(client_dir.path().join(name)).await?;
            assert!(
                downloaded == torrent.content[*offset..offset + len],
                "{name} differs from libtorrent's"
            );
        }
        assert_eq!(handle.file_priorities(), vec![1, 0, 7]);

        lt.send(&json!({"cmd": "quit"})).await?;
        let requests = lt.expect("requests", WAIT).await?;
        let pieces: Vec<u64> = requests
            .get("pieces")
            .and_then(Value::as_array)
            .context("no pieces in the requests event")?
            .iter()
            .map(|v| v.as_u64().context("piece is not a number"))
            .collect::<anyhow::Result<_>>()?;
        assert_eq!(
            pieces.iter().copied().collect::<BTreeSet<_>>(),
            BTreeSet::from([0, 1, 2, 5, 6]),
            "libtorrent was asked for a piece of the skipped file alone: {pieces:?}"
        );
        // "c" is at maximum, so its pieces (5 is shared with the skipped "b") go first.
        assert_eq!(
            pieces.iter().take(2).copied().collect::<BTreeSet<_>>(),
            BTreeSet::from([5, 6]),
            "the maximum file was not asked for first: {pieces:?}"
        );
        session.stop().await;
        Ok(())
    })
    .await
}
