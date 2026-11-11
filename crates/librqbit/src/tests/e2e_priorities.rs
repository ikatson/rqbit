//! Download priorities of files and pieces, end to end: which pieces a session asks a seeder for,
//! in which order, what happens when levels change on a live torrent, and how they are stored.
//!
//! The seeder is hand-written: it listens on loopback, records every request and cancel it
//! receives and serves the blocks right away, unless told to hold some pieces back. Pieces are one
//! block long, so one request is one piece. The client is a real session that dials the seeder,
//! so a client that dropped the seeder because it had everything it wanted dials it again when it
//! wants more.

use std::{
    collections::HashSet,
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, bail};
use bytes::Bytes;
use parking_lot::Mutex;
use sha1w::{ISha1, Sha1};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::{JoinHandle, JoinSet},
    time::{Instant, timeout},
};

use crate::{
    AddTorrent, AddTorrentOptions, Session, SessionOptions, SessionPersistenceConfig,
    api::TorrentIdOrHash,
    tests::fake_peer::{FakePeer, Recv, Wire, run},
    torrent_state::ManagedTorrentHandle,
};

pub(super) const PIECE: u32 = 16 * 1024;
pub(super) const NAME: &str = "prio";

// ---------------------------------------------------------------------------------------------
// The torrent
// ---------------------------------------------------------------------------------------------

// Shared with the libtorrent interop test of priorities.
pub(super) struct Torrent {
    pub(super) bytes: Bytes,
    // All files back to back.
    pub(super) content: Arc<Vec<u8>>,
    // (name, offset, length) of every file, in torrent order.
    pub(super) files: Vec<(String, usize, usize)>,
}

impl Torrent {
    fn pieces(&self) -> u32 {
        u32::try_from(self.content.len().div_ceil(PIECE as usize)).unwrap()
    }

    fn piece_data(&self, piece: u32) -> &[u8] {
        let start = piece as usize * PIECE as usize;
        let end = (start + PIECE as usize).min(self.content.len());
        &self.content[start..end]
    }
}

fn bstr(b: &[u8]) -> Vec<u8> {
    let mut o = format!("{}:", b.len()).into_bytes();
    o.extend_from_slice(b);
    o
}

fn bint(v: usize) -> Vec<u8> {
    format!("i{v}e").into_bytes()
}

/// A multi-file torrent named [`NAME`] whose files are called "a", "b", "c"... in this order,
/// with the given lengths and pseudo-random content.
pub(super) fn make_torrent(lens: &[usize]) -> Torrent {
    let total: usize = lens.iter().sum();
    // xorshift: different data in every piece, so a misplaced block fails the hash check.
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let content: Vec<u8> = (0..total)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x.to_le_bytes()[0]
        })
        .collect();

    let mut pieces = Vec::new();
    for chunk in content.chunks(PIECE as usize) {
        let mut h = Sha1::new();
        h.update(chunk);
        pieces.extend_from_slice(&h.finish());
    }

    let mut files = Vec::new();
    let mut flist = vec![b'l'];
    let mut offset = 0;
    for (i, len) in lens.iter().copied().enumerate() {
        let name = char::from(b'a' + u8::try_from(i).unwrap()).to_string();
        flist.push(b'd');
        flist.extend(bstr(b"length"));
        flist.extend(bint(len));
        flist.extend(bstr(b"path"));
        flist.push(b'l');
        flist.extend(bstr(name.as_bytes()));
        flist.extend(b"ee");
        files.push((name, offset, len));
        offset += len;
    }
    flist.push(b'e');

    let mut t = b"d4:infod".to_vec();
    t.extend(bstr(b"files"));
    t.extend(flist);
    t.extend(bstr(b"name"));
    t.extend(bstr(NAME.as_bytes()));
    t.extend(bstr(b"piece length"));
    t.extend(bint(PIECE as usize));
    t.extend(bstr(b"pieces"));
    t.extend(bstr(&pieces));
    t.extend(b"ee");

    Torrent {
        bytes: t.into(),
        content: Arc::new(content),
        files,
    }
}

// ---------------------------------------------------------------------------------------------
// The seeder
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    Request(u32),
    Cancel(u32),
}

#[derive(Default)]
struct SeederLog {
    seen: Vec<Seen>,
    connections: usize,
    // Pieces whose requests are recorded but not answered.
    hold: HashSet<u32>,
}

struct Seeder {
    addr: SocketAddr,
    log: Arc<Mutex<SeederLog>>,
    task: JoinHandle<()>,
}

impl Drop for Seeder {
    fn drop(&mut self) {
        // Dropping the accept task drops its JoinSet, which aborts every connection.
        self.task.abort();
    }
}

impl Seeder {
    fn requests(&self) -> Vec<u32> {
        self.log
            .lock()
            .seen
            .iter()
            .filter_map(|s| match s {
                Seen::Request(p) => Some(*p),
                Seen::Cancel(_) => None,
            })
            .collect()
    }

    fn requested(&self) -> HashSet<u32> {
        self.requests().into_iter().collect()
    }

    fn connections(&self) -> usize {
        self.log.lock().connections
    }
}

async fn start_seeder(torrent: &Torrent) -> anyhow::Result<Seeder> {
    start_seeder_holding(torrent, HashSet::new()).await
}

async fn start_seeder_holding(torrent: &Torrent, hold: HashSet<u32>) -> anyhow::Result<Seeder> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let addr = listener.local_addr()?;
    let log = Arc::new(Mutex::new(SeederLog {
        hold,
        ..Default::default()
    }));
    let content = torrent.content.clone();
    let pieces = torrent.pieces();
    let task = tokio::spawn({
        let log = log.clone();
        async move {
            let mut conns = JoinSet::new();
            while let Ok((stream, _)) = listener.accept().await {
                log.lock().connections += 1;
                let local = match stream.local_addr() {
                    Ok(a) => a,
                    Err(_) => continue,
                };
                let peer = FakePeer {
                    stream,
                    buf: Vec::new(),
                    local,
                };
                conns.spawn(serve(peer, content.clone(), pieces, log.clone()));
            }
        }
    });
    Ok(Seeder { addr, log, task })
}

/// One connection of the seeder: answers the handshake, says it has everything, unchokes and
/// serves what it is asked for until the other side goes away.
async fn serve(mut p: FakePeer, content: Arc<Vec<u8>>, pieces: u32, log: Arc<Mutex<SeederLog>>) {
    let mut theirs = [0u8; 68];
    if p.stream.read_exact(&mut theirs).await.is_err() {
        return;
    }
    let mut ours = [0u8; 68];
    ours[..28].copy_from_slice(&theirs[..28]);
    // No extensions: the session talks plain BEP 3 to us.
    ours[20..28].fill(0);
    ours[28..48].copy_from_slice(&theirs[28..48]);
    ours[48..68].copy_from_slice(b"-FAKE00-prioseeder00");
    let mut bits = vec![0u8; pieces.div_ceil(8) as usize];
    for i in 0..pieces as usize {
        bits[i / 8] |= 0x80 >> (i % 8);
    }
    if p.stream.write_all(&ours).await.is_err()
        || p.bitfield(&bits).await.is_err()
        || p.unchoke().await.is_err()
    {
        return;
    }
    loop {
        match p.recv(Duration::from_secs(120)).await {
            Recv::Msg(Wire::Request(index, begin, len)) => {
                let held = {
                    let mut log = log.lock();
                    log.seen.push(Seen::Request(index));
                    log.hold.contains(&index)
                };
                if held {
                    continue;
                }
                let start = index as usize * PIECE as usize + begin as usize;
                let Some(data) = content.get(start..start + len as usize) else {
                    return;
                };
                if p.piece(index, begin, data).await.is_err() {
                    return;
                }
            }
            Recv::Msg(Wire::Cancel(index, _, _)) => log.lock().seen.push(Seen::Cancel(index)),
            Recv::Msg(_) => {}
            Recv::Timeout | Recv::Closed => return,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------------------------

struct Client {
    session: Arc<Session>,
    handle: ManagedTorrentHandle,
    dir: TempDir,
}

pub(super) fn client_session_opts(persistence: Option<SessionPersistenceConfig>) -> SessionOptions {
    SessionOptions {
        dht: None,
        persistence,
        listen: None,
        ipv4_only: true,
        disable_local_service_discovery: true,
        ..Default::default()
    }
}

/// A session downloading `torrent` from `seeder` into an empty folder.
async fn start_client(
    torrent: &Torrent,
    seeder: Option<&Seeder>,
    opts: AddTorrentOptions,
) -> anyhow::Result<Client> {
    let dir = TempDir::with_prefix("rqbit_priorities_client")?;
    let session = Session::new_with_opts(dir.path().into(), client_session_opts(None))
        .await
        .context("error creating client session")?;
    let handle = add(&session, &dir, torrent, seeder, opts).await?;
    Ok(Client {
        session,
        handle,
        dir,
    })
}

async fn add(
    session: &Arc<Session>,
    dir: &TempDir,
    torrent: &Torrent,
    seeder: Option<&Seeder>,
    opts: AddTorrentOptions,
) -> anyhow::Result<ManagedTorrentHandle> {
    let handle = session
        .add_torrent(
            AddTorrent::from_bytes(torrent.bytes.clone()),
            Some(AddTorrentOptions {
                output_folder: Some(dir.path().to_str().context("temp dir path")?.to_owned()),
                overwrite: true,
                initial_peers: seeder.map(|s| vec![s.addr]),
                ..opts
            }),
        )
        .await?
        .into_handle()
        .context("into_handle")?;
    handle.wait_until_initialized().await?;
    Ok(handle)
}

impl Client {
    async fn wait_completed(&self) -> anyhow::Result<()> {
        timeout(Duration::from_secs(30), self.handle.wait_until_completed())
            .await
            .context("torrent did not complete")?
    }

    /// The bytes of `piece` as the client wrote them.
    fn read_piece(&self, torrent: &Torrent, piece: u32) -> anyhow::Result<Vec<u8>> {
        let start = piece as usize * PIECE as usize;
        let end = (start + PIECE as usize).min(torrent.content.len());
        let mut out = Vec::new();
        for (name, offset, len) in &torrent.files {
            let (lo, hi) = (start.max(*offset), end.min(offset + len));
            if lo >= hi {
                continue;
            }
            // An explicit output folder holds the files themselves, not a folder named after
            // the torrent.
            let data = std::fs::read(self.dir.path().join(name))
                .with_context(|| format!("reading {name}"))?;
            out.extend_from_slice(
                data.get(lo - offset..hi - offset)
                    .with_context(|| format!("{name} is too short"))?,
            );
        }
        Ok(out)
    }

    fn assert_has(&self, torrent: &Torrent, pieces: impl IntoIterator<Item = u32>) {
        for p in pieces {
            assert_eq!(
                self.read_piece(torrent, p).unwrap(),
                torrent.piece_data(p),
                "piece {p} differs from the seeder's"
            );
        }
    }
}

/// Polls `cond` until it holds or `within` runs out.
async fn wait_for(within: Duration, what: &str, cond: impl Fn() -> bool) -> anyhow::Result<()> {
    let deadline = Instant::now() + within;
    while !cond() {
        if Instant::now() > deadline {
            bail!("timed out waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}

fn set(pieces: impl IntoIterator<Item = u32>) -> HashSet<u32> {
    pieces.into_iter().collect()
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_maximum_file_is_downloaded_before_normal_ones() -> anyhow::Result<()> {
    run(async {
        let p = PIECE as usize;
        // Pieces 0..4 are "a", 4..12 "b", 12..16 "c".
        let torrent = make_torrent(&[4 * p, 8 * p, 4 * p]);
        let seeder = start_seeder(&torrent).await?;
        let client = start_client(
            &torrent,
            Some(&seeder),
            AddTorrentOptions {
                file_priorities: Some(vec![1, 7, 1]),
                ..Default::default()
            },
        )
        .await?;
        client.wait_completed().await?;

        // The seeder answers in the order it is asked, so this is also the order the pieces
        // completed in.
        let requests = seeder.requests();
        assert_eq!(
            set(requests.iter().copied().take(8)),
            set(4..12),
            "the maximum file's pieces must be asked for first: {requests:?}"
        );
        assert_eq!(seeder.requested(), set(0..16));
        client.assert_has(&torrent, 0..16);
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_skipped_file_is_never_requested_but_its_boundary_pieces_are() -> anyhow::Result<()> {
    run(async {
        let p = PIECE as usize;
        // "a" is pieces 0 and 1, "b" 1, 2 and 3, "c" 3 and 4: "b" shares a piece with each
        // neighbour, and only piece 2 is "b"'s alone.
        let torrent = make_torrent(&[p + p / 2, 2 * p, p + p / 2]);
        let seeder = start_seeder(&torrent).await?;
        let client = start_client(
            &torrent,
            Some(&seeder),
            AddTorrentOptions {
                file_priorities: Some(vec![1, 0, 1]),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(client.handle.file_priorities(), vec![1, 0, 1]);
        assert_eq!(client.handle.piece_priorities(), vec![1, 1, 0, 1, 1]);
        assert_eq!(client.handle.only_files(), Some(vec![0, 2]));
        client.wait_completed().await?;

        assert_eq!(seeder.requested(), set([0, 1, 3, 4]));
        client.assert_has(&torrent, [0, 1, 3, 4]);
        let stats = client.handle.stats();
        assert!(stats.finished);
        assert_eq!(stats.total_bytes, 4 * PIECE as u64);
        assert_eq!(stats.progress_bytes, stats.total_bytes);
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_raising_a_skipped_file_downloads_it_before_completing() -> anyhow::Result<()> {
    run(async {
        let p = PIECE as usize;
        let torrent = make_torrent(&[2 * p, 2 * p]);
        let seeder = start_seeder(&torrent).await?;
        let client = start_client(
            &torrent,
            Some(&seeder),
            AddTorrentOptions {
                file_priorities: Some(vec![1, 0]),
                ..Default::default()
            },
        )
        .await?;
        client.wait_completed().await?;
        assert_eq!(seeder.requested(), set([0, 1]));
        // Finished: the client has no use for a seeder any more and hangs up.
        wait_for(Duration::from_secs(10), "the seeder to be dropped", || {
            client.handle.live().is_some_and(|l| {
                l.per_peer_stats_snapshot(Default::default())
                    .peers
                    .is_empty()
            })
        })
        .await?;

        client
            .session
            .set_file_priorities(&client.handle, &[(1, 1)])
            .await?;
        assert!(!client.handle.stats().finished);
        client.wait_completed().await?;

        // Completion was only reported once the raised file was on disk, through a new
        // connection to the seeder.
        client.assert_has(&torrent, 0..4);
        assert_eq!(seeder.requested(), set(0..4));
        assert!(seeder.connections() >= 2);
        let stats = client.handle.stats();
        assert_eq!(stats.progress_bytes, 4 * PIECE as u64);
        assert_eq!(stats.total_bytes, 4 * PIECE as u64);
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lowering_a_file_to_zero_cancels_its_requests() -> anyhow::Result<()> {
    run(async {
        let p = PIECE as usize;
        let torrent = make_torrent(&[4 * p, 4 * p]);
        // "b"'s pieces are asked for but never delivered.
        let seeder = start_seeder_holding(&torrent, set(4..8)).await?;
        let client = start_client(&torrent, Some(&seeder), Default::default()).await?;
        wait_for(
            Duration::from_secs(20),
            "requests for every piece of b",
            || seeder.requested().is_superset(&set(4..8)),
        )
        .await?;

        client
            .session
            .set_file_priorities(&client.handle, &[(1, 0)])
            .await?;
        wait_for(
            Duration::from_secs(10),
            "a cancel for every piece of b",
            || {
                let seen = seeder.log.lock().seen.clone();
                (4..8).all(|piece| seen.contains(&Seen::Cancel(piece)))
            },
        )
        .await?;
        client.wait_completed().await?;

        // Nothing of "b" is asked for after it was cancelled, and "a" was not disturbed.
        let seen = seeder.log.lock().seen.clone();
        let first_cancel = seen
            .iter()
            .position(|s| matches!(s, Seen::Cancel(_)))
            .context("no cancel")?;
        assert!(
            !seen[first_cancel..]
                .iter()
                .any(|s| matches!(s, Seen::Request(p) if *p >= 4)),
            "b requested again after the cancel: {seen:?}"
        );
        assert!(
            !seen.iter().any(|s| matches!(s, Seen::Cancel(p) if *p < 4)),
            "a wanted piece was cancelled: {seen:?}"
        );
        client.assert_has(&torrent, 0..4);
        assert_eq!(
            client.handle.piece_priorities(),
            vec![1, 1, 1, 1, 0, 0, 0, 0]
        );
        let stats = client.handle.stats();
        assert!(stats.finished);
        assert_eq!(stats.progress_bytes, 4 * PIECE as u64);
        assert_eq!(stats.total_bytes, 4 * PIECE as u64);
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_piece_overrides_beat_file_levels() -> anyhow::Result<()> {
    run(async {
        let p = PIECE as usize;
        let torrent = make_torrent(&[2 * p, 2 * p]);
        let seeder = start_seeder(&torrent).await?;
        let client = start_client(
            &torrent,
            Some(&seeder),
            AddTorrentOptions {
                file_priorities: Some(vec![1, 0]),
                // Piece 0 above everything, piece 3 wanted although its file is skipped.
                piece_priorities: Some(vec![(0, 7), (3, 1)]),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(client.handle.piece_priorities(), vec![7, 1, 0, 1]);
        client.wait_completed().await?;
        assert_eq!(seeder.requests().first(), Some(&0));
        assert_eq!(seeder.requested(), set([0, 1, 3]));

        // Live: piece 2 gets an override, piece 3 loses its one. Piece 3 stays on disk.
        client
            .session
            .set_piece_priorities(&client.handle, &[(2, Some(1)), (3, None)])
            .await?;
        assert_eq!(client.handle.piece_priorities(), vec![7, 1, 1, 0]);
        assert_eq!(client.handle.file_priorities(), vec![1, 0]);
        assert!(!client.handle.stats().finished);
        client.wait_completed().await?;
        assert_eq!(seeder.requested(), set(0..4));
        client.assert_has(&torrent, 0..4);
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_priorities_at_add_time_together_with_only_files() -> anyhow::Result<()> {
    run(async {
        let p = PIECE as usize;
        let torrent = make_torrent(&[2 * p, 2 * p, 2 * p]);
        let seeder = start_seeder(&torrent).await?;
        let client = start_client(
            &torrent,
            Some(&seeder),
            AddTorrentOptions {
                only_files: Some(vec![0, 1]),
                // "a" is selected, so its 0 becomes normal; "c" is not, so its 7 becomes 0.
                file_priorities: Some(vec![0, 7, 7]),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(client.handle.file_priorities(), vec![1, 7, 0]);
        assert_eq!(client.handle.only_files(), Some(vec![0, 1]));
        client.wait_completed().await?;
        let requests = seeder.requests();
        assert_eq!(set(requests.iter().copied().take(2)), set([2, 3]));
        assert_eq!(seeder.requested(), set(0..4));
        client.assert_has(&torrent, 0..4);
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_invalid_priorities_are_rejected_and_change_nothing() -> anyhow::Result<()> {
    run(async {
        let p = PIECE as usize;
        let torrent = make_torrent(&[2 * p, 2 * p]);
        let dir = TempDir::with_prefix("rqbit_priorities_client")?;
        let session = Session::new_with_opts(dir.path().into(), client_session_opts(None)).await?;
        let paused = || AddTorrentOptions {
            paused: true,
            ..Default::default()
        };
        for bad in [
            AddTorrentOptions {
                file_priorities: Some(vec![1, 1, 1]),
                ..paused()
            },
            AddTorrentOptions {
                file_priorities: Some(vec![8]),
                ..paused()
            },
            AddTorrentOptions {
                piece_priorities: Some(vec![(4, 1)]),
                ..paused()
            },
        ] {
            assert!(add(&session, &dir, &torrent, None, bad).await.is_err());
        }

        // A shorter list leaves the rest normal.
        let handle = add(
            &session,
            &dir,
            &torrent,
            None,
            AddTorrentOptions {
                file_priorities: Some(vec![6]),
                ..paused()
            },
        )
        .await?;
        assert_eq!(handle.file_priorities(), vec![6, 1]);

        // A partial update with one bad entry is refused as a whole.
        assert!(
            session
                .set_file_priorities(&handle, &[(0, 7), (2, 1)])
                .await
                .is_err()
        );
        assert!(
            session
                .set_file_priorities(&handle, &[(0, 7), (1, 8)])
                .await
                .is_err()
        );
        assert!(
            session
                .set_piece_priorities(&handle, &[(0, Some(7)), (4, Some(1))])
                .await
                .is_err()
        );
        assert_eq!(handle.file_priorities(), vec![6, 1]);
        assert_eq!(handle.piece_priorities(), vec![6, 6, 1, 1]);
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_update_only_files_is_expressed_in_priorities() -> anyhow::Result<()> {
    run(async {
        let p = PIECE as usize;
        let torrent = make_torrent(&[2 * p, 2 * p, 2 * p]);
        let seeder = start_seeder(&torrent).await?;
        let client = start_client(
            &torrent,
            Some(&seeder),
            AddTorrentOptions {
                paused: true,
                file_priorities: Some(vec![7, 1, 1]),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(client.handle.only_files(), None);

        let session = &client.session;
        session
            .update_only_files(&client.handle, &HashSet::from([0, 2]))
            .await?;
        // Kept files keep their level.
        assert_eq!(client.handle.file_priorities(), vec![7, 0, 1]);
        assert_eq!(client.handle.only_files(), Some(vec![0, 2]));

        session
            .update_only_files(&client.handle, &HashSet::from([0]))
            .await?;
        session
            .update_only_files(&client.handle, &HashSet::from([0, 1]))
            .await?;
        // A file brought back from 0 is normal.
        assert_eq!(client.handle.file_priorities(), vec![7, 1, 0]);
        assert_eq!(client.handle.only_files(), Some(vec![0, 1]));

        // And it still selects what is downloaded.
        session.unpause(&client.handle).await?;
        client.wait_completed().await?;
        assert_eq!(seeder.requested(), set(0..4));
        Ok(())
    })
    .await
}

/// Starts a session on the JSON store in `dir/session` and returns the torrent it restored.
async fn restore(
    dir: &TempDir,
    torrent: &Torrent,
) -> anyhow::Result<(Arc<Session>, ManagedTorrentHandle)> {
    let session = Session::new_with_opts(
        dir.path().into(),
        client_session_opts(Some(SessionPersistenceConfig::Json {
            folder: Some(dir.path().join("session")),
        })),
    )
    .await?;
    let info_hash = crate::torrent_from_bytes(&torrent.bytes)?.info_hash;
    let handle = session
        .get(TorrentIdOrHash::Hash(info_hash))
        .context("torrent was not restored")?;
    Ok((session, handle))
}

#[tokio::test(flavor = "multi_thread")]
async fn test_priorities_survive_a_session_restart() -> anyhow::Result<()> {
    run(async {
        let p = PIECE as usize;
        let torrent = make_torrent(&[2 * p, 2 * p, 2 * p]);
        let dir = TempDir::with_prefix("rqbit_priorities_persist")?;
        let session = Session::new_with_opts(
            dir.path().into(),
            client_session_opts(Some(SessionPersistenceConfig::Json {
                folder: Some(dir.path().join("session")),
            })),
        )
        .await?;
        let handle = add(
            &session,
            &dir,
            &torrent,
            None,
            AddTorrentOptions {
                paused: true,
                file_priorities: Some(vec![7, 0, 1]),
                ..Default::default()
            },
        )
        .await?;
        session.set_file_priorities(&handle, &[(2, 6)]).await?;
        session
            .set_piece_priorities(&handle, &[(2, Some(1)), (5, Some(0))])
            .await?;
        assert_eq!(handle.piece_priorities(), vec![7, 7, 1, 0, 6, 0]);
        session.stop().await;
        drop(handle);
        drop(session);

        let (session, handle) = restore(&dir, &torrent).await?;
        assert_eq!(handle.file_priorities(), vec![7, 0, 6]);
        assert_eq!(handle.piece_priorities(), vec![7, 7, 1, 0, 6, 0]);
        assert_eq!(handle.only_files(), Some(vec![0, 2]));
        session.stop().await;
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_store_written_without_priorities_loads_only_files_as_levels() -> anyhow::Result<()>
{
    run(async {
        let p = PIECE as usize;
        let torrent = make_torrent(&[2 * p, 2 * p, 2 * p]);
        let dir = TempDir::with_prefix("rqbit_priorities_legacy")?;
        let session = Session::new_with_opts(
            dir.path().into(),
            client_session_opts(Some(SessionPersistenceConfig::Json {
                folder: Some(dir.path().join("session")),
            })),
        )
        .await?;
        add(
            &session,
            &dir,
            &torrent,
            None,
            AddTorrentOptions {
                paused: true,
                file_priorities: Some(vec![7, 0, 1]),
                piece_priorities: Some(vec![(2, 6)]),
                ..Default::default()
            },
        )
        .await?;
        session.stop().await;
        drop(session);

        // What a version without priorities wrote: the same entry minus the new fields.
        let db = dir.path().join("session").join("session.json");
        let mut json: serde_json::Value = serde_json::from_slice(&std::fs::read(&db)?)?;
        let torrents = json
            .get_mut("torrents")
            .and_then(|t| t.as_object_mut())
            .context("no torrents in the store")?;
        assert_eq!(torrents.len(), 1);
        for t in torrents.values_mut() {
            let t = t.as_object_mut().context("torrent entry")?;
            assert_eq!(t.get("only_files"), Some(&serde_json::json!([0, 2])));
            assert!(t.remove("file_priorities").is_some());
            assert!(t.remove("piece_priorities").is_some());
        }
        std::fs::write(&db, serde_json::to_vec(&json)?)?;

        let (session, handle) = restore(&dir, &torrent).await?;
        assert_eq!(handle.file_priorities(), vec![1, 0, 1]);
        assert_eq!(handle.piece_priorities(), vec![1, 1, 0, 0, 1, 1]);
        assert_eq!(handle.only_files(), Some(vec![0, 2]));
        session.stop().await;
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_closed_streams_piece_is_given_back_when_the_seeder_is_dropped() -> anyhow::Result<()>
{
    run(async {
        let p = PIECE as usize;
        // "a" is pieces 0 and 1, "b" (skipped) 2 and 3. Piece 1 and 2 are asked for but not
        // delivered until the end.
        let torrent = make_torrent(&[2 * p, 2 * p]);
        let seeder = start_seeder_holding(&torrent, set([1, 2])).await?;
        let client = start_client(
            &torrent,
            Some(&seeder),
            AddTorrentOptions {
                file_priorities: Some(vec![1, 0]),
                ..Default::default()
            },
        )
        .await?;
        wait_for(Duration::from_secs(20), "requests for a", || {
            seeder.requested().is_superset(&set([0, 1]))
        })
        .await?;

        // A player opens "b" and gets stuck on its first piece, then goes away.
        let mut stream = client.handle.clone().stream(1).await?;
        let reader = tokio::spawn(async move {
            let mut byte = [0u8; 1];
            stream.read(&mut byte).await
        });
        wait_for(Duration::from_secs(20), "the stream's request", || {
            seeder.requested().contains(&2)
        })
        .await?;
        reader.abort();
        assert!(reader.await.is_err_and(|e| e.is_cancelled()));

        // Nothing is wanted any more: the torrent finishes and hangs up on the seeder while
        // the stream's piece is still in flight with it.
        client
            .session
            .set_file_priorities(&client.handle, &[(0, 0)])
            .await?;
        wait_for(Duration::from_secs(10), "the seeder to be dropped", || {
            client.handle.stats().finished
                && client.handle.live().is_some_and(|l| {
                    l.per_peer_stats_snapshot(Default::default())
                        .peers
                        .is_empty()
                })
        })
        .await?;
        // The piece is given back with the connection. Were it left reserved for it, only the
        // death of a later connection to the same address would free it.
        let piece_2 = librqbit_core::lengths::Lengths::new(torrent.content.len() as u64, PIECE)?
            .validate_piece_index(2)
            .context("piece 2")?;
        wait_for(
            Duration::from_secs(10),
            "the stream's piece to be released",
            || {
                client.handle.live().is_some_and(|l| {
                    l.lock_read("test")
                        .get_pieces()
                        .is_ok_and(|p| p.get_inflight(piece_2).is_none())
                })
            },
        )
        .await?;

        // Wanting everything again downloads everything, the stream's piece included: it was
        // not left reserved for the connection that is gone.
        seeder.log.lock().hold.clear();
        client
            .session
            .set_file_priorities(&client.handle, &[(0, 1), (1, 1)])
            .await?;
        client.wait_completed().await?;
        client.assert_has(&torrent, 0..4);
        Ok(())
    })
    .await
}
