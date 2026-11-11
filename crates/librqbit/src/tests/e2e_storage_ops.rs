//! End-to-end tests of the storage operations: `Session::force_recheck`,
//! `AddTorrentOptions::skip_initial_check`, `Session::move_storage` and `Session::rename_file`.
//!
//! They run real sessions on loopback. Torrents are built here, so that file layouts (nested
//! directories, BEP 47 padding) and piece boundaries are known exactly. A storage wrapper
//! counts reads (to tell whether a check hashed anything), can fail writes, and can hold a
//! read or a move at a gate so that something else can happen exactly then.

use std::{
    future::Future,
    net::{Ipv4Addr, SocketAddr},
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, bail};
use bitvec::{order::Msb0, slice::BitSlice};
use bytes::Bytes;
use sha1w::{ISha1, Sha1};
use tempfile::TempDir;
use tokio::time::timeout;

use crate::{
    AddTorrent, AddTorrentOptions, ConnectionOptions, ListenerOptions, ManagedTorrent,
    ManagedTorrentShared, ManagedTorrentState, MoveStorageMode, Session, SessionOptions,
    SessionPersistenceConfig, TorrentMetadata, TorrentStatsState,
    api::TorrentIdOrHash,
    limits::LimitsConfig,
    storage::{
        BoxStorageFactory, MoveStorageOutcome, MoveStorageRequest, StorageFactory,
        StorageFactoryExt, TorrentStorage, filesystem::FilesystemStorageFactory,
    },
    tests::test_util::{setup_test_logging, wait_until},
    torrent_state::ManagedTorrentHandle,
};

const PIECE_LEN: usize = 16 * 1024;
// Fast enough to keep the tests short, slow enough that a transfer of TRANSFER_FILES is
// still running seconds after it started.
const SLOW_BPS: u32 = 128 * 1024;
// 4 pieces, a bit less than 3 pieces (with `pad`, a padding file follows it) and 2.5 pieces,
// in nested directories.
const NESTED_FILES: &[(&str, usize)] = &[
    ("a.bin", 4 * PIECE_LEN),
    ("sub/deeper/b.bin", 3 * PIECE_LEN - 1000),
    ("sub/c.bin", 5 * PIECE_LEN / 2),
];
// Two files of 2 pieces each and one of 2 pieces, for the skip-check tests.
const SIX_PIECES: &[(&str, usize)] = &[
    ("one.bin", 2 * PIECE_LEN),
    ("two.bin", 2 * PIECE_LEN),
    ("three.bin", 2 * PIECE_LEN),
];
// About 0.5 MiB: several seconds at SLOW_BPS.
const TRANSFER_FILES: &[(&str, usize)] = &[
    ("a.bin", 200_000),
    ("sub/deeper/b.bin", 150_000),
    ("sub/c.bin", 174_288),
];
const WAIT: Duration = Duration::from_secs(30);

async fn run(fut: impl Future<Output = anyhow::Result<()>>) -> anyhow::Result<()> {
    setup_test_logging();
    timeout(Duration::from_secs(90), fut)
        .await
        .context("test timed out")?
}

async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) -> anyhow::Result<()> {
    wait_until(
        || {
            if cond() { Ok(()) } else { bail!("{what}") }
        },
        WAIT,
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// Torrent content
// ---------------------------------------------------------------------------------------------

struct TestFile {
    // Index in the torrent (padding files count).
    index: usize,
    path: &'static str,
    offset: usize,
    data: Vec<u8>,
}

struct Content {
    files: Vec<TestFile>,
    total_len: usize,
    torrent: Bytes,
}

impl Content {
    // A multi-file torrent of random content. With `pad`, every file but the last is
    // followed by a BEP 47 padding file up to the next piece boundary.
    fn new(files: &[(&'static str, usize)], pad: bool) -> Self {
        let mut out = Vec::new();
        let mut entries = Vec::new();
        let mut stream = Vec::new();
        for (i, (path, len)) in files.iter().enumerate() {
            let mut data = vec![0u8; *len];
            rand::fill(&mut data[..]);
            out.push(TestFile {
                index: entries.len(),
                path,
                offset: stream.len(),
                data: data.clone(),
            });
            entries.push(file_entry(path.split('/').collect(), *len, false));
            stream.extend_from_slice(&data);
            let rem = len % PIECE_LEN;
            if pad && rem != 0 && i + 1 < files.len() {
                let pad_len = PIECE_LEN - rem;
                let pad_name = pad_len.to_string();
                entries.push(file_entry(vec![".pad", &pad_name], pad_len, true));
                stream.resize(stream.len() + pad_len, 0);
            }
        }
        let mut pieces = Vec::new();
        for piece in stream.chunks(PIECE_LEN) {
            let mut h = Sha1::new();
            h.update(piece);
            pieces.extend_from_slice(&h.finish());
        }
        let mut flist = vec![b'l'];
        for e in entries {
            flist.extend_from_slice(&e);
        }
        flist.push(b'e');
        let info = bdict(&[
            (b"files".as_slice(), flist),
            (b"name".as_slice(), bstr(b"storage-ops")),
            (b"piece length".as_slice(), bint(PIECE_LEN as i64)),
            (b"pieces".as_slice(), bstr(&pieces)),
        ]);
        let torrent = bdict(&[(b"info".as_slice(), info)]);
        Self {
            files: out,
            total_len: stream.len(),
            torrent: torrent.into(),
        }
    }

    fn num_pieces(&self) -> usize {
        self.total_len.div_ceil(PIECE_LEN)
    }

    fn all_pieces(&self) -> Vec<usize> {
        (0..self.num_pieces()).collect()
    }

    fn file(&self, path: &str) -> &TestFile {
        self.files.iter().find(|f| f.path == path).unwrap()
    }

    // The pieces that lie entirely inside the given files.
    fn pieces_of(&self, paths: &[&str]) -> Vec<usize> {
        let mut out = Vec::new();
        for f in self.files.iter().filter(|f| paths.contains(&f.path)) {
            let first = f.offset.div_ceil(PIECE_LEN);
            let end = (f.offset + f.data.len()) / PIECE_LEN;
            let end = if f.offset + f.data.len() == self.total_len {
                self.num_pieces()
            } else {
                end
            };
            out.extend(first..end);
        }
        out.sort_unstable();
        out
    }

    fn write_to(&self, dir: &Path) {
        for f in &self.files {
            let p = dir.join(f.path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, &f.data).unwrap();
        }
    }

    // Every file is at `dir`, under its own path unless `renamed` gives another one, with the
    // right content.
    fn assert_at(&self, dir: &Path, renamed: &[(usize, &str)]) -> anyhow::Result<()> {
        for f in &self.files {
            let rel = renamed
                .iter()
                .find(|(i, _)| *i == f.index)
                .map(|(_, p)| *p)
                .unwrap_or(f.path);
            let p = dir.join(rel);
            let data = std::fs::read(&p).with_context(|| format!("reading {p:?}"))?;
            if data != f.data {
                bail!("{p:?} doesn't have the expected content");
            }
        }
        Ok(())
    }

    // Overwrites a few bytes inside `piece`.
    fn corrupt(&self, dir: &Path, piece: usize) {
        use std::io::{Seek, SeekFrom, Write};
        let at = piece * PIECE_LEN + 100;
        let f = self
            .files
            .iter()
            .find(|f| f.offset <= at && at < f.offset + f.data.len())
            .unwrap();
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(dir.join(f.path))
            .unwrap();
        file.seek(SeekFrom::Start((at - f.offset) as u64)).unwrap();
        let orig = f.data[at - f.offset];
        file.write_all(&[!orig, !orig, !orig]).unwrap();
        file.sync_all().unwrap();
    }
}

fn file_entry(path: Vec<&str>, len: usize, padding: bool) -> Vec<u8> {
    let mut path_enc = vec![b'l'];
    for bit in path {
        path_enc.extend_from_slice(&bstr(bit.as_bytes()));
    }
    path_enc.push(b'e');
    let mut entries: Vec<(&[u8], Vec<u8>)> = Vec::new();
    if padding {
        entries.push((b"attr".as_slice(), bstr(b"p")));
    }
    entries.push((b"length".as_slice(), bint(len as i64)));
    entries.push((b"path".as_slice(), path_enc));
    bdict(&entries)
}

fn bstr(b: &[u8]) -> Vec<u8> {
    let mut o = format!("{}:", b.len()).into_bytes();
    o.extend_from_slice(b);
    o
}

fn bint(v: i64) -> Vec<u8> {
    format!("i{v}e").into_bytes()
}

fn bdict(entries: &[(&[u8], Vec<u8>)]) -> Vec<u8> {
    let mut o = vec![b'd'];
    for (k, v) in entries {
        o.extend_from_slice(&bstr(k));
        o.extend_from_slice(v);
    }
    o.push(b'e');
    o
}

// ---------------------------------------------------------------------------------------------
// Instrumented storage
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct GateState {
    armed: bool,
    entered: bool,
    released: bool,
}

// Holds the first caller after `arm` until `release`.
#[derive(Default)]
struct Gate {
    state: Mutex<GateState>,
    cond: Condvar,
}

impl Gate {
    fn arm(&self) {
        *self.state.lock().unwrap() = GateState {
            armed: true,
            entered: false,
            released: false,
        };
    }

    fn pass(&self) {
        let mut g = self.state.lock().unwrap();
        if !g.armed {
            return;
        }
        g.armed = false;
        g.entered = true;
        self.cond.notify_all();
        // Bounded, so that a failing test can't leave a blocking thread behind forever.
        let (g, _) = self
            .cond
            .wait_timeout_while(g, WAIT, |g| !g.released)
            .unwrap();
        drop(g);
    }

    fn release(&self) {
        self.state.lock().unwrap().released = true;
        self.cond.notify_all();
    }

    async fn wait_entered(self: &Arc<Self>) -> anyhow::Result<()> {
        let gate = self.clone();
        let entered = tokio::task::spawn_blocking(move || {
            let g = gate.state.lock().unwrap();
            let (g, _) = gate
                .cond
                .wait_timeout_while(g, WAIT, |g| !g.entered)
                .unwrap();
            g.entered
        })
        .await?;
        if !entered {
            bail!("nothing reached the gate");
        }
        Ok(())
    }
}

#[derive(Default)]
struct Ctl {
    read_bytes: AtomicU64,
    fail_writes: AtomicBool,
    read_gate: Arc<Gate>,
    move_gate: Arc<Gate>,
}

#[derive(Clone, Default)]
struct InstrumentedFactory {
    ctl: Arc<Ctl>,
}

struct Instrumented {
    inner: Box<dyn TorrentStorage>,
    ctl: Arc<Ctl>,
}

impl StorageFactory for InstrumentedFactory {
    type Storage = Instrumented;

    fn create(
        &self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<Instrumented> {
        Ok(Instrumented {
            inner: Box::new(FilesystemStorageFactory::default().create(shared, metadata)?),
            ctl: self.ctl.clone(),
        })
    }

    fn is_type_id(&self, type_id: std::any::TypeId) -> bool {
        FilesystemStorageFactory::default().is_type_id(type_id)
    }

    fn clone_box(&self) -> BoxStorageFactory {
        self.clone().boxed()
    }
}

impl Instrumented {
    fn check_write(&self) -> anyhow::Result<()> {
        if self.ctl.fail_writes.load(Ordering::SeqCst) {
            bail!("injected write failure");
        }
        Ok(())
    }
}

impl TorrentStorage for Instrumented {
    fn init(
        &mut self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        self.inner.init(shared, metadata)
    }

    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        self.ctl.read_gate.pass();
        self.ctl
            .read_bytes
            .fetch_add(buf.len() as u64, Ordering::SeqCst);
        self.inner.pread_exact(file_id, offset, buf)
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        self.check_write()?;
        self.inner.pwrite_all(file_id, offset, buf)
    }

    fn pwrite_all_vectored(
        &self,
        file_id: usize,
        offset: u64,
        bufs: [std::io::IoSlice<'_>; 2],
    ) -> anyhow::Result<usize> {
        self.check_write()?;
        self.inner.pwrite_all_vectored(file_id, offset, bufs)
    }

    fn remove_file(&self, file_id: usize, filename: &Path) -> anyhow::Result<()> {
        self.inner.remove_file(file_id, filename)
    }

    fn remove_directory_if_empty(&self, path: &Path) -> anyhow::Result<()> {
        self.inner.remove_directory_if_empty(path)
    }

    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()> {
        self.inner.ensure_file_length(file_id, length)
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        Ok(Box::new(Instrumented {
            inner: self.inner.take()?,
            ctl: self.ctl.clone(),
        }))
    }

    fn file_length(&self, file_id: usize) -> anyhow::Result<Option<u64>> {
        self.inner.file_length(file_id)
    }

    fn rename_file(&self, file_id: usize, from: &Path, to: &Path) -> anyhow::Result<()> {
        self.inner.rename_file(file_id, from, to)
    }

    fn move_storage(&self, request: &MoveStorageRequest<'_>) -> anyhow::Result<MoveStorageOutcome> {
        self.ctl.move_gate.pass();
        self.inner.move_storage(request)
    }

    fn finish_move_storage(&self, commit: bool) -> anyhow::Result<()> {
        self.inner.finish_move_storage(commit)
    }
}

// ---------------------------------------------------------------------------------------------
// Sessions and torrents
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct SessionOpts {
    listen: bool,
    // JSON persistence (with fastresume) in this folder.
    persistence: Option<PathBuf>,
    upload_bps: Option<u32>,
    download_bps: Option<u32>,
}

async fn new_session(dir: &Path, o: SessionOpts) -> anyhow::Result<Arc<Session>> {
    let fastresume = o.persistence.is_some();
    Session::new_with_opts(
        dir.to_owned(),
        SessionOptions {
            dht: None,
            persistence: o.persistence.map(|folder| SessionPersistenceConfig::Json {
                folder: Some(folder),
            }),
            fastresume,
            listen: o.listen.then(|| ListenerOptions {
                listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
                ipv4_only: true,
                ..Default::default()
            }),
            connect: Some(ConnectionOptions::default()),
            ipv4_only: true,
            disable_local_service_discovery: true,
            ratelimits: LimitsConfig {
                upload_bps: o.upload_bps.and_then(NonZeroU32::new),
                download_bps: o.download_bps.and_then(NonZeroU32::new),
            },
            ..Default::default()
        },
    )
    .await
    .context("error creating session")
}

// Adds `content` stored in `folder` and waits for its check.
async fn add(
    session: &Arc<Session>,
    content: &Content,
    folder: &Path,
    opts: AddTorrentOptions,
) -> anyhow::Result<ManagedTorrentHandle> {
    let handle = session
        .add_torrent(
            AddTorrent::from_bytes(content.torrent.clone()),
            Some(AddTorrentOptions {
                output_folder: Some(folder.to_str().context("utf-8")?.to_owned()),
                overwrite: true,
                ..opts
            }),
        )
        .await?
        .into_handle()
        .context("into_handle")?;
    wait_initialized(&handle).await?;
    Ok(handle)
}

async fn wait_initialized(handle: &ManagedTorrent) -> anyhow::Result<()> {
    timeout(WAIT, handle.wait_until_initialized())
        .await
        .context("timed out waiting for the check")?
}

async fn wait_completed(handle: &ManagedTorrent) -> anyhow::Result<()> {
    timeout(WAIT, handle.wait_until_completed())
        .await
        .context("timed out waiting for the download")?
}

fn have_pieces(handle: &ManagedTorrent) -> anyhow::Result<Vec<usize>> {
    handle.with_chunk_tracker(|ct| ct.get_have_pieces().as_slice().iter_ones().collect())
}

fn state(handle: &ManagedTorrent) -> TorrentStatsState {
    handle.stats().state
}

fn is_live(handle: &ManagedTorrent) -> bool {
    matches!(state(handle), TorrentStatsState::Live)
}

fn is_paused(handle: &ManagedTorrent) -> bool {
    matches!(state(handle), TorrentStatsState::Paused)
}

fn check_running(handle: &ManagedTorrent) -> bool {
    handle.with_state(|s| matches!(s, ManagedTorrentState::Initializing(i) if i.is_check_running()))
}

// A complete seeder of `content`, listening on loopback.
struct Seeder {
    session: Arc<Session>,
    handle: ManagedTorrentHandle,
    addr: SocketAddr,
    dir: TempDir,
}

async fn start_seeder(content: &Content, upload_bps: Option<u32>) -> anyhow::Result<Seeder> {
    let dir = TempDir::with_prefix("rqbit_storage_ops_seed")?;
    content.write_to(dir.path());
    let session = new_session(
        dir.path(),
        SessionOpts {
            listen: true,
            upload_bps,
            ..Default::default()
        },
    )
    .await?;
    let handle = add(&session, content, dir.path(), Default::default()).await?;
    wait_completed(&handle).await?;
    let addr = session.listen_addr().context("seeder doesn't listen")?;
    Ok(Seeder {
        session,
        handle,
        addr,
        dir,
    })
}

// A session downloading `content` from `seed` into `<dir>/out`.
struct Client {
    session: Arc<Session>,
    handle: ManagedTorrentHandle,
    dir: TempDir,
    ctl: Arc<Ctl>,
}

impl Client {
    fn out(&self) -> PathBuf {
        self.dir.path().join("out")
    }
}

async fn start_client(
    content: &Content,
    seed: &Seeder,
    download_bps: Option<u32>,
    paused: bool,
) -> anyhow::Result<Client> {
    let dir = TempDir::with_prefix("rqbit_storage_ops_client")?;
    let session = new_session(
        dir.path(),
        SessionOpts {
            download_bps,
            ..Default::default()
        },
    )
    .await?;
    let ctl = Arc::new(Ctl::default());
    let handle = add(
        &session,
        content,
        &dir.path().join("out"),
        AddTorrentOptions {
            paused,
            initial_peers: Some(vec![seed.addr]),
            storage_factory: Some(InstrumentedFactory { ctl: ctl.clone() }.boxed()),
            ..Default::default()
        },
    )
    .await?;
    Ok(Client {
        session,
        handle,
        dir,
        ctl,
    })
}

// Waits until the client has some of the content, but not all of it.
async fn wait_mid_transfer(handle: &ManagedTorrent, total: usize) -> anyhow::Result<()> {
    wait_for("some progress", || {
        handle.stats().progress_bytes >= 4 * PIECE_LEN as u64
    })
    .await?;
    let stats = handle.stats();
    if stats.finished || stats.progress_bytes >= total as u64 {
        bail!("the transfer is already over, the test can't do anything in the middle of it");
    }
    Ok(())
}

// The have-bitfield in the `.bitv` file that JSON persistence keeps in `folder`.
fn stored_bitfield(folder: &Path) -> anyhow::Result<Vec<usize>> {
    let mut found = None;
    for e in std::fs::read_dir(folder)? {
        let p = e?.path();
        if p.extension().is_some_and(|e| e == "bitv") {
            found = Some(p);
        }
    }
    let bytes = std::fs::read(found.context("no .bitv file")?)?;
    Ok(BitSlice::<u8, Msb0>::from_slice(&bytes)
        .iter_ones()
        .collect())
}

fn without(all: Vec<usize>, missing: &[usize]) -> Vec<usize> {
    all.into_iter().filter(|p| !missing.contains(p)).collect()
}

// ---------------------------------------------------------------------------------------------
// Force recheck
// ---------------------------------------------------------------------------------------------

// A seeding torrent whose file got damaged in two pieces: after the recheck exactly those two
// are missing, and they are downloaded again from another seeder (and nothing else is).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_recheck_seeding_redownloads_damaged_pieces() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, false);
        let other = start_seeder(&content, None).await?;

        let dir = TempDir::with_prefix("rqbit_recheck_seed")?;
        content.write_to(dir.path());
        let session = new_session(dir.path(), Default::default()).await?;
        let handle = add(
            &session,
            &content,
            dir.path(),
            AddTorrentOptions {
                initial_peers: Some(vec![other.addr]),
                ..Default::default()
            },
        )
        .await?;
        wait_completed(&handle).await?;
        assert_eq!(have_pieces(&handle)?, content.all_pieces());

        content.corrupt(dir.path(), 1);
        content.corrupt(dir.path(), 5);
        session.force_recheck(&handle).await?;
        wait_initialized(&handle).await?;
        assert!(is_live(&handle));
        // The torrent may already be downloading them again: look at the stored bitfield
        // only until then.
        let have = have_pieces(&handle)?;
        assert!(
            have.len() >= content.num_pieces() - 2,
            "pieces other than the damaged ones were lost: {have:?}"
        );

        wait_completed(&handle).await?;
        content.assert_at(dir.path(), &[])?;
        let fetched = handle
            .live()
            .context("not live")?
            .stats_snapshot()
            .fetched_bytes;
        assert!(
            fetched >= 2 * PIECE_LEN as u64 && fetched <= 4 * PIECE_LEN as u64,
            "expected to fetch the two damaged pieces again, fetched {fetched} bytes"
        );
        drop(other);
        Ok(())
    })
    .await
}

// Without a peer, the recheck's result stays: exactly the damaged pieces are missing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_recheck_finds_exactly_the_damaged_pieces() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let dir = TempDir::with_prefix("rqbit_recheck_exact")?;
        content.write_to(dir.path());
        let session = new_session(dir.path(), Default::default()).await?;
        let handle = add(&session, &content, dir.path(), Default::default()).await?;
        assert_eq!(have_pieces(&handle)?, content.all_pieces());

        content.corrupt(dir.path(), 2);
        content.corrupt(dir.path(), 7);
        session.force_recheck(&handle).await?;
        wait_initialized(&handle).await?;
        assert!(is_live(&handle));
        assert_eq!(
            have_pieces(&handle)?,
            without(content.all_pieces(), &[2, 7])
        );
        assert!(!handle.stats().finished);
        Ok(())
    })
    .await
}

// A complete paused torrent is rechecked (really: its data is read again) and stays paused
// and complete.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_recheck_paused_stays_paused() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, false);
        let dir = TempDir::with_prefix("rqbit_recheck_paused")?;
        content.write_to(dir.path());
        let session = new_session(dir.path(), Default::default()).await?;
        let ctl = Arc::new(Ctl::default());
        let handle = add(
            &session,
            &content,
            dir.path(),
            AddTorrentOptions {
                paused: true,
                storage_factory: Some(InstrumentedFactory { ctl: ctl.clone() }.boxed()),
                ..Default::default()
            },
        )
        .await?;
        assert!(is_paused(&handle));
        let read_before = ctl.read_bytes.load(Ordering::SeqCst);

        // Hold the check on its first read, to see the state it reports meanwhile.
        ctl.read_gate.arm();
        session.force_recheck(&handle).await?;
        ctl.read_gate.wait_entered().await?;
        assert!(matches!(
            state(&handle),
            TorrentStatsState::Initializing { paused: true }
        ));
        ctl.read_gate.release();
        wait_initialized(&handle).await?;
        assert!(is_paused(&handle), "{:?}", state(&handle));
        assert!(handle.is_paused());
        assert!(handle.stats().finished);
        assert_eq!(have_pieces(&handle)?, content.all_pieces());
        let read = ctl.read_bytes.load(Ordering::SeqCst) - read_before;
        assert!(
            read >= content.total_len as u64,
            "the recheck read {read} bytes, less than the content"
        );
        Ok(())
    })
    .await
}

// A torrent in the middle of a download: the recheck keeps what was verified, the counters
// carry on, and the download continues to the end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_recheck_downloading_keeps_progress_and_counters() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(TRANSFER_FILES, false);
        let seed = start_seeder(&content, Some(SLOW_BPS)).await?;
        let client = start_client(&content, &seed, None, false).await?;
        wait_mid_transfer(&client.handle, content.total_len).await?;

        let before = client.handle.live().context("not live")?.stats_snapshot();
        let have_before = have_pieces(&client.handle)?;
        client.session.force_recheck(&client.handle).await?;
        wait_initialized(&client.handle).await?;
        let after = client
            .handle
            .live()
            .context("not live after the recheck")?
            .stats_snapshot();
        assert!(
            after.downloaded_and_checked_bytes >= before.downloaded_and_checked_bytes,
            "{after:?} < {before:?}"
        );
        assert!(after.fetched_bytes >= before.fetched_bytes);
        assert!(after.uploaded_bytes >= before.uploaded_bytes);
        let have_after = have_pieces(&client.handle)?;
        for p in &have_before {
            assert!(have_after.contains(p), "piece {p} was lost by the recheck");
        }

        wait_completed(&client.handle).await?;
        content.assert_at(&client.out(), &[])?;
        Ok(())
    })
    .await
}

// A pause during a recheck is honored like during the initial check: the check stops, the
// torrent stays initializing (paused), and resuming finishes the check and starts it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_recheck_pause_during_check() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, false);
        let dir = TempDir::with_prefix("rqbit_recheck_pause")?;
        content.write_to(dir.path());
        let session = new_session(dir.path(), Default::default()).await?;
        let ctl = Arc::new(Ctl::default());
        let handle = add(
            &session,
            &content,
            dir.path(),
            AddTorrentOptions {
                storage_factory: Some(InstrumentedFactory { ctl: ctl.clone() }.boxed()),
                ..Default::default()
            },
        )
        .await?;
        assert!(is_live(&handle));

        ctl.read_gate.arm();
        session.force_recheck(&handle).await?;
        ctl.read_gate.wait_entered().await?;
        assert!(matches!(
            state(&handle),
            TorrentStatsState::Initializing { paused: false }
        ));
        session.pause(&handle).await?;
        ctl.read_gate.release();
        wait_for("the check to stop", || !check_running(&handle)).await?;
        assert!(
            matches!(
                state(&handle),
                TorrentStatsState::Initializing { paused: true }
            ),
            "{:?}",
            state(&handle)
        );

        session.unpause(&handle).await?;
        wait_initialized(&handle).await?;
        assert!(is_live(&handle));
        assert!(handle.stats().finished);
        assert_eq!(have_pieces(&handle)?, content.all_pieces());
        Ok(())
    })
    .await
}

// A torrent that went to error (a write failed) is rechecked, leaves the error and finishes
// its download.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_recheck_errored_torrent() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(TRANSFER_FILES, false);
        let seed = start_seeder(&content, Some(SLOW_BPS)).await?;
        let client = start_client(&content, &seed, None, true).await?;
        client.ctl.fail_writes.store(true, Ordering::SeqCst);
        client.session.unpause(&client.handle).await?;
        wait_for("the torrent to fail", || {
            matches!(state(&client.handle), TorrentStatsState::Error)
        })
        .await?;

        client.ctl.fail_writes.store(false, Ordering::SeqCst);
        client.session.force_recheck(&client.handle).await?;
        wait_initialized(&client.handle).await?;
        assert!(is_live(&client.handle), "{:?}", state(&client.handle));
        wait_completed(&client.handle).await?;
        content.assert_at(&client.out(), &[])?;
        Ok(())
    })
    .await
}

// The bitfield stored for fastresume is the rechecked one, and a restarted session uses it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_recheck_bitfield_survives_restart() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, false);
        let dir = TempDir::with_prefix("rqbit_recheck_restart")?;
        let data = dir.path().join("data");
        let db = dir.path().join("db");
        content.write_to(&data);
        let opts = || SessionOpts {
            persistence: Some(db.clone()),
            ..Default::default()
        };
        let session = new_session(dir.path(), opts()).await?;
        let handle = add(&session, &content, &data, Default::default()).await?;
        assert_eq!(stored_bitfield(&db)?, content.all_pieces());

        content.corrupt(&data, 3);
        session.force_recheck(&handle).await?;
        wait_initialized(&handle).await?;
        let expected = without(content.all_pieces(), &[3]);
        assert_eq!(have_pieces(&handle)?, expected);
        assert_eq!(stored_bitfield(&db)?, expected);
        let info_hash = handle.info_hash();
        session.stop().await;
        drop(handle);
        drop(session);

        let session = new_session(dir.path(), opts()).await?;
        let handle = session
            .get(TorrentIdOrHash::Hash(info_hash))
            .context("torrent not restored")?;
        wait_initialized(&handle).await?;
        assert_eq!(have_pieces(&handle)?, expected);
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Skip initial check
// ---------------------------------------------------------------------------------------------

// Complete content is trusted without reading a byte, and can be downloaded from at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_skip_check_complete_content_is_seeded_without_hashing() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let dir = TempDir::with_prefix("rqbit_skip_check")?;
        content.write_to(dir.path());
        let session = new_session(
            dir.path(),
            SessionOpts {
                listen: true,
                ..Default::default()
            },
        )
        .await?;
        let ctl = Arc::new(Ctl::default());
        let handle = add(
            &session,
            &content,
            dir.path(),
            AddTorrentOptions {
                skip_initial_check: true,
                storage_factory: Some(InstrumentedFactory { ctl: ctl.clone() }.boxed()),
                ..Default::default()
            },
        )
        .await?;
        assert!(handle.stats().finished);
        assert_eq!(have_pieces(&handle)?, content.all_pieces());
        assert_eq!(ctl.read_bytes.load(Ordering::SeqCst), 0, "data was hashed");

        let seed = Seeder {
            addr: session.listen_addr().context("no listen addr")?,
            session,
            handle,
            dir,
        };
        let client = start_client(&content, &seed, None, false).await?;
        wait_completed(&client.handle).await?;
        content.assert_at(&client.out(), &[])?;
        Ok(())
    })
    .await
}

// A missing file, or one with another length, makes the torrent hash everything, which finds
// exactly what is there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_skip_check_falls_back_to_a_full_check() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(SIX_PIECES, false);
        for case in ["missing", "short"] {
            let dir = TempDir::with_prefix("rqbit_skip_check_fallback")?;
            content.write_to(dir.path());
            let two = dir.path().join("two.bin");
            let expected = match case {
                "missing" => {
                    std::fs::remove_file(&two)?;
                    content.pieces_of(&["one.bin", "three.bin"])
                }
                _ => {
                    // Half of it: its first piece is still good.
                    let f = std::fs::OpenOptions::new().write(true).open(&two)?;
                    f.set_len(PIECE_LEN as u64)?;
                    let mut p = content.pieces_of(&["one.bin", "three.bin"]);
                    p.push(content.pieces_of(&["two.bin"])[0]);
                    p.sort_unstable();
                    p
                }
            };
            let session = new_session(dir.path(), Default::default()).await?;
            let ctl = Arc::new(Ctl::default());
            let handle = add(
                &session,
                &content,
                dir.path(),
                AddTorrentOptions {
                    paused: true,
                    skip_initial_check: true,
                    storage_factory: Some(InstrumentedFactory { ctl: ctl.clone() }.boxed()),
                    ..Default::default()
                },
            )
            .await?;
            assert_eq!(have_pieces(&handle)?, expected, "{case}");
            assert!(
                ctl.read_bytes.load(Ordering::SeqCst) > 0,
                "{case}: nothing was hashed"
            );
        }
        Ok(())
    })
    .await
}

// With a selection, only the selected files have to be complete: an unselected missing file
// doesn't prevent skipping the check, a selected one does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_skip_check_with_only_files() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(SIX_PIECES, false);
        let selected = vec![0, 2];

        // The unselected file is missing: skipped, the selected files' pieces are had.
        let dir = TempDir::with_prefix("rqbit_skip_check_only")?;
        content.write_to(dir.path());
        std::fs::remove_file(dir.path().join("two.bin"))?;
        let session = new_session(dir.path(), Default::default()).await?;
        let ctl = Arc::new(Ctl::default());
        let handle = add(
            &session,
            &content,
            dir.path(),
            AddTorrentOptions {
                paused: true,
                skip_initial_check: true,
                only_files: Some(selected.clone()),
                storage_factory: Some(InstrumentedFactory { ctl: ctl.clone() }.boxed()),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(
            have_pieces(&handle)?,
            content.pieces_of(&["one.bin", "three.bin"])
        );
        assert!(handle.stats().finished);
        assert_eq!(ctl.read_bytes.load(Ordering::SeqCst), 0, "data was hashed");

        // A selected file is missing: full check.
        let dir = TempDir::with_prefix("rqbit_skip_check_only_missing")?;
        content.write_to(dir.path());
        std::fs::remove_file(dir.path().join("two.bin"))?;
        std::fs::remove_file(dir.path().join("three.bin"))?;
        let session = new_session(dir.path(), Default::default()).await?;
        let ctl = Arc::new(Ctl::default());
        let handle = add(
            &session,
            &content,
            dir.path(),
            AddTorrentOptions {
                paused: true,
                skip_initial_check: true,
                only_files: Some(selected),
                storage_factory: Some(InstrumentedFactory { ctl: ctl.clone() }.boxed()),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(have_pieces(&handle)?, content.pieces_of(&["one.bin"]));
        assert!(
            ctl.read_bytes.load(Ordering::SeqCst) > 0,
            "nothing was hashed"
        );
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Move storage
// ---------------------------------------------------------------------------------------------

fn no_torrent_files_at(content: &Content, dir: &Path) -> anyhow::Result<()> {
    for f in &content.files {
        if dir.join(f.path).exists() {
            bail!("{:?} is still there", dir.join(f.path));
        }
    }
    if dir.join(".pad").exists() {
        bail!("a padding file was materialized at {dir:?}");
    }
    Ok(())
}

// A paused torrent with nested directories and padding files: everything moves, the empty
// directories at the old place go, nothing is hashed and the torrent stays paused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_move_storage_paused() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let dir = TempDir::with_prefix("rqbit_move_paused")?;
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        content.write_to(&old);
        let session = new_session(dir.path(), Default::default()).await?;
        let ctl = Arc::new(Ctl::default());
        let handle = add(
            &session,
            &content,
            &old,
            AddTorrentOptions {
                paused: true,
                storage_factory: Some(InstrumentedFactory { ctl: ctl.clone() }.boxed()),
                ..Default::default()
            },
        )
        .await?;
        let read_before = ctl.read_bytes.load(Ordering::SeqCst);

        session
            .move_storage(&handle, new.clone(), MoveStorageMode::FailIfExists)
            .await?;
        assert_eq!(handle.output_folder(), new);
        content.assert_at(&new, &[])?;
        no_torrent_files_at(&content, &old)?;
        assert!(!old.join("sub").exists(), "empty directories were left");
        assert!(!new.join(".pad").exists());
        assert!(is_paused(&handle));
        assert!(handle.stats().finished);
        assert_eq!(
            ctl.read_bytes.load(Ordering::SeqCst),
            read_before,
            "rehashed"
        );

        // The storage works at the new place.
        session.force_recheck(&handle).await?;
        wait_initialized(&handle).await?;
        assert_eq!(have_pieces(&handle)?, content.all_pieces());
        Ok(())
    })
    .await
}

// Moving the storage of a torrent in the middle of its download: the download completes at
// the new place.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_move_storage_downloading() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(TRANSFER_FILES, true);
        let seed = start_seeder(&content, Some(SLOW_BPS)).await?;
        let client = start_client(&content, &seed, None, false).await?;
        wait_mid_transfer(&client.handle, content.total_len).await?;

        let new = client.dir.path().join("moved");
        client
            .session
            .move_storage(&client.handle, new.clone(), MoveStorageMode::FailIfExists)
            .await?;
        assert!(is_live(&client.handle));
        wait_completed(&client.handle).await?;
        content.assert_at(&new, &[])?;
        no_torrent_files_at(&content, &client.out())?;
        Ok(())
    })
    .await
}

// Moving the storage of a seeder while a peer downloads from it: the peer finishes, the
// seeder's counters carry on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_move_storage_seeding() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(TRANSFER_FILES, false);
        let seed = start_seeder(&content, Some(SLOW_BPS)).await?;
        let client = start_client(&content, &seed, None, false).await?;
        wait_mid_transfer(&client.handle, content.total_len).await?;

        let uploaded_before = seed.handle.live().context("not live")?.get_uploaded_bytes();
        let new = TempDir::with_prefix("rqbit_move_seeding_dst")?;
        let new = new.path().join("moved");
        seed.session
            .move_storage(&seed.handle, new.clone(), MoveStorageMode::FailIfExists)
            .await?;
        assert!(is_live(&seed.handle));
        assert!(seed.handle.stats().finished);
        content.assert_at(&new, &[])?;
        no_torrent_files_at(&content, seed.dir.path())?;

        wait_completed(&client.handle).await?;
        content.assert_at(&client.out(), &[])?;
        let uploaded_after = seed.handle.live().context("not live")?.get_uploaded_bytes();
        assert!(uploaded_after >= uploaded_before);
        Ok(())
    })
    .await
}

// The three modes when a file of the torrent already exists at the destination.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_move_storage_modes_with_existing_destination() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let a = content.file("a.bin");
        for (mode, same_len) in [
            (MoveStorageMode::FailIfExists, true),
            (MoveStorageMode::Overwrite, true),
            (MoveStorageMode::KeepExisting, true),
            (MoveStorageMode::KeepExisting, false),
        ] {
            let what = format!("{mode:?}, same length: {same_len}");
            let dir = TempDir::with_prefix("rqbit_move_modes")?;
            let old = dir.path().join("old");
            let new = dir.path().join("new");
            content.write_to(&old);
            std::fs::create_dir_all(&new)?;
            let existing = vec![0x5a; if same_len { a.data.len() } else { 1000 }];
            std::fs::write(new.join("a.bin"), &existing)?;

            let session = new_session(dir.path(), Default::default()).await?;
            let handle = add(
                &session,
                &content,
                &old,
                AddTorrentOptions {
                    paused: true,
                    ..Default::default()
                },
            )
            .await?;
            let result = session.move_storage(&handle, new.clone(), mode).await;
            match mode {
                MoveStorageMode::FailIfExists => {
                    assert!(result.is_err(), "{what}");
                    assert_eq!(handle.output_folder(), old, "{what}");
                    content.assert_at(&old, &[])?;
                    assert_eq!(std::fs::read(new.join("a.bin"))?, existing, "{what}");
                    assert!(!new.join("sub").exists(), "{what}");
                    assert!(is_paused(&handle), "{what}");
                    assert_eq!(have_pieces(&handle)?, content.all_pieces(), "{what}");
                }
                MoveStorageMode::Overwrite => {
                    result?;
                    content.assert_at(&new, &[])?;
                    no_torrent_files_at(&content, &old)?;
                    assert_eq!(have_pieces(&handle)?, content.all_pieces(), "{what}");
                }
                MoveStorageMode::KeepExisting => {
                    result?;
                    wait_initialized(&handle).await?;
                    assert_eq!(handle.output_folder(), new, "{what}");
                    assert!(is_paused(&handle), "{what}");
                    let len = std::fs::metadata(new.join("a.bin"))?.len();
                    if same_len {
                        assert_eq!(std::fs::read(new.join("a.bin"))?, existing, "{what}");
                    } else {
                        // The check gave the adopted file the torrent's length.
                        assert_eq!(len, a.data.len() as u64, "{what}");
                    }
                    // The destination holds other bytes than the source: only the same
                    // bytes make the source redundant, so it stays.
                    assert_eq!(
                        std::fs::read(old.join("a.bin"))?,
                        a.data,
                        "{what}: source kept"
                    );
                    // The adopted file was rechecked: its pieces are missing, the others
                    // moved and are still had.
                    let lost = content.pieces_of(&["a.bin"]);
                    assert_eq!(
                        have_pieces(&handle)?,
                        without(content.all_pieces(), &lost),
                        "{what}"
                    );
                    for f in content.files.iter().filter(|f| f.path != "a.bin") {
                        assert_eq!(std::fs::read(new.join(f.path))?, f.data, "{what}");
                        assert!(!old.join(f.path).exists(), "{what}");
                    }
                }
            }
        }
        Ok(())
    })
    .await
}

// A move that fails part-way (a destination directory can't be written to) puts back what
// it moved, and the torrent keeps working at the old place.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_move_storage_failure_rolls_back() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let dir = TempDir::with_prefix("rqbit_move_rollback")?;
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        content.write_to(&old);
        // a.bin moves, then sub/deeper/b.bin can't.
        let locked = new.join("sub");
        std::fs::create_dir_all(&locked)?;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555))?;

        let session = new_session(dir.path(), Default::default()).await?;
        let handle = add(&session, &content, &old, Default::default()).await?;
        assert!(is_live(&handle));
        let result = session
            .move_storage(&handle, new.clone(), MoveStorageMode::FailIfExists)
            .await;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))?;

        let err = result.err().context("the move succeeded")?;
        assert!(
            !format!("{err:#}").contains("undoing"),
            "the rollback failed: {err:#}"
        );
        assert_eq!(handle.output_folder(), old);
        content.assert_at(&old, &[])?;
        no_torrent_files_at(&content, &new)?;
        assert!(is_live(&handle));
        assert_eq!(have_pieces(&handle)?, content.all_pieces());

        // Still usable at the old place.
        session.force_recheck(&handle).await?;
        wait_initialized(&handle).await?;
        assert_eq!(have_pieces(&handle)?, content.all_pieces());
        Ok(())
    })
    .await
}

// The new folder is persisted: a restarted session finds the torrent there, complete.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_move_storage_survives_restart() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let dir = TempDir::with_prefix("rqbit_move_restart")?;
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        let db = dir.path().join("db");
        content.write_to(&old);
        let opts = || SessionOpts {
            persistence: Some(db.clone()),
            ..Default::default()
        };
        let session = new_session(dir.path(), opts()).await?;
        let handle = add(&session, &content, &old, Default::default()).await?;
        session
            .move_storage(&handle, new.clone(), MoveStorageMode::FailIfExists)
            .await?;
        let info_hash = handle.info_hash();
        session.stop().await;
        drop(handle);
        drop(session);

        let session = new_session(dir.path(), opts()).await?;
        let handle = session
            .get(TorrentIdOrHash::Hash(info_hash))
            .context("torrent not restored")?;
        wait_initialized(&handle).await?;
        assert_eq!(handle.output_folder(), new);
        assert_eq!(have_pieces(&handle)?, content.all_pieces());
        assert_eq!(stored_bitfield(&db)?, content.all_pieces());
        content.assert_at(&new, &[])?;
        no_torrent_files_at(&content, &old)?;
        Ok(())
    })
    .await
}

// Moving to the current folder does nothing; moving into a sub-folder of the current one and
// back out of it works.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_move_storage_same_and_nested_folders() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let dir = TempDir::with_prefix("rqbit_move_nested")?;
        let old = dir.path().join("old");
        content.write_to(&old);
        let session = new_session(dir.path(), Default::default()).await?;
        let handle = add(&session, &content, &old, Default::default()).await?;

        session
            .move_storage(&handle, old.clone(), MoveStorageMode::FailIfExists)
            .await?;
        assert_eq!(handle.output_folder(), old);
        content.assert_at(&old, &[])?;
        assert!(is_live(&handle));

        // Into old/sub: "sub/c.bin" ends up at old/sub/sub/c.bin, nothing collides.
        let inner = old.join("sub");
        session
            .move_storage(&handle, inner.clone(), MoveStorageMode::FailIfExists)
            .await?;
        assert_eq!(handle.output_folder(), inner);
        content.assert_at(&inner, &[])?;
        assert!(!old.join("a.bin").exists());

        // And back out.
        session
            .move_storage(&handle, old.clone(), MoveStorageMode::FailIfExists)
            .await?;
        assert_eq!(handle.output_folder(), old);
        content.assert_at(&old, &[])?;
        assert!(!inner.join("a.bin").exists());
        assert!(!inner.join("sub").exists());
        assert!(is_live(&handle));
        assert_eq!(have_pieces(&handle)?, content.all_pieces());
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Rename
// ---------------------------------------------------------------------------------------------

// Renamed before the download starts: the data is written under the new name.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_rename_before_download() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let seed = start_seeder(&content, None).await?;
        let client = start_client(&content, &seed, None, true).await?;
        let a = content.file("a.bin").index;
        client
            .session
            .rename_file(&client.handle, a, "x/y/renamed.bin".into())
            .await?;
        assert_eq!(
            client.handle.file_path(a),
            Some(PathBuf::from("x/y/renamed.bin"))
        );
        client.session.unpause(&client.handle).await?;
        wait_completed(&client.handle).await?;
        content.assert_at(&client.out(), &[(a, "x/y/renamed.bin")])?;
        assert!(!client.out().join("a.bin").exists());
        Ok(())
    })
    .await
}

// Renamed on both sides while the transfer runs: on the downloading side, and on the
// seeding side. The download completes and both have the file under the new name.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_rename_mid_download_and_while_seeding() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(TRANSFER_FILES, true);
        let seed = start_seeder(&content, Some(SLOW_BPS)).await?;
        let client = start_client(&content, &seed, None, false).await?;
        wait_mid_transfer(&client.handle, content.total_len).await?;
        let b = content.file("sub/deeper/b.bin").index;
        let c = content.file("sub/c.bin").index;

        client
            .session
            .rename_file(&client.handle, b, "new-dir/b.bin".into())
            .await?;
        seed.session
            .rename_file(&seed.handle, c, "c-renamed.bin".into())
            .await?;
        assert!(is_live(&client.handle));
        assert!(is_live(&seed.handle));
        assert!(!seed.dir.path().join("sub/c.bin").exists());
        // b.bin left sub/deeper empty.
        assert!(!client.out().join("sub/deeper").exists());

        wait_completed(&client.handle).await?;
        content.assert_at(&client.out(), &[(b, "new-dir/b.bin")])?;
        content.assert_at(seed.dir.path(), &[(c, "c-renamed.bin")])?;
        Ok(())
    })
    .await
}

// Paths that are rejected, before anything changes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_rename_rejections() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let dir = TempDir::with_prefix("rqbit_rename_reject")?;
        content.write_to(dir.path());
        let session = new_session(dir.path(), Default::default()).await?;
        let handle = add(&session, &content, dir.path(), Default::default()).await?;
        let a = content.file("a.bin").index;
        let c = content.file("sub/c.bin").index;
        // The padding file after b.bin.
        let pad = content.file("sub/deeper/b.bin").index + 1;
        assert!(handle.with_metadata(|m| m.file_infos[pad].attrs.padding)?);
        let file_count = handle.with_metadata(|m| m.file_infos.len())?;

        let mut rejected: Vec<(usize, PathBuf)> = vec![
            // Another file, in another case.
            (a, "SUB/C.BIN".into()),
            // A directory holding other files.
            (a, "sub".into()),
            (a, "Sub/Deeper".into()),
            // Under a file.
            (c, "a.bin/c.bin".into()),
            (a, "../escaped.bin".into()),
            (a, "sub/../../escaped.bin".into()),
            (a, "".into()),
            (pad, "pad.bin".into()),
            (file_count, "nope.bin".into()),
        ];
        rejected.push((a, dir.path().join("absolute.bin")));
        for (idx, path) in rejected {
            let r = session.rename_file(&handle, idx, path.clone()).await;
            assert!(r.is_err(), "renaming file {idx} to {path:?} was accepted");
        }
        assert!(handle.shared().renamed_files().is_empty());
        content.assert_at(dir.path(), &[])?;
        assert!(is_live(&handle));

        // Renaming it to its own path in another case is a rename.
        session.rename_file(&handle, a, "A.bin".into()).await?;
        assert_eq!(handle.file_path(a), Some(PathBuf::from("A.bin")));
        Ok(())
    })
    .await
}

// The new path survives a restart; deleting the torrent with its files removes the renamed
// file and the directories it created.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_rename_survives_restart_and_delete_removes_it() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let dir = TempDir::with_prefix("rqbit_rename_restart")?;
        let data = dir.path().join("data");
        let db = dir.path().join("db");
        content.write_to(&data);
        let opts = || SessionOpts {
            persistence: Some(db.clone()),
            ..Default::default()
        };
        let session = new_session(dir.path(), opts()).await?;
        let handle = add(&session, &content, &data, Default::default()).await?;
        let a = content.file("a.bin").index;
        session
            .rename_file(&handle, a, "x/y/renamed.bin".into())
            .await?;
        let info_hash = handle.info_hash();
        session.stop().await;
        drop(handle);
        drop(session);

        let session = new_session(dir.path(), opts()).await?;
        let handle = session
            .get(TorrentIdOrHash::Hash(info_hash))
            .context("torrent not restored")?;
        wait_initialized(&handle).await?;
        assert_eq!(handle.file_path(a), Some(PathBuf::from("x/y/renamed.bin")));
        assert_eq!(have_pieces(&handle)?, content.all_pieces());
        content.assert_at(&data, &[(a, "x/y/renamed.bin")])?;

        session.delete(handle.id().into(), true).await?;
        assert!(
            !data.join("x").exists(),
            "the renamed file or its directories remain"
        );
        no_torrent_files_at(&content, &data)?;
        Ok(())
    })
    .await
}

// Rename, then move the storage, then recheck: every step uses the new path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_rename_then_move_then_recheck() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let dir = TempDir::with_prefix("rqbit_rename_move")?;
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        content.write_to(&old);
        let session = new_session(dir.path(), Default::default()).await?;
        let handle = add(&session, &content, &old, Default::default()).await?;
        let b = content.file("sub/deeper/b.bin").index;
        session
            .rename_file(&handle, b, "elsewhere/b2.bin".into())
            .await?;
        session
            .move_storage(&handle, new.clone(), MoveStorageMode::FailIfExists)
            .await?;
        content.assert_at(&new, &[(b, "elsewhere/b2.bin")])?;
        assert!(!old.join("elsewhere").exists());
        assert!(!old.join("sub").exists());

        session.force_recheck(&handle).await?;
        wait_initialized(&handle).await?;
        assert_eq!(have_pieces(&handle)?, content.all_pieces());
        assert!(handle.stats().finished);
        assert!(!new.join("sub/deeper/b.bin").exists());
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------------------------

// Two operations issued at once on one torrent run one after the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_two_operations_at_once() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let dir = TempDir::with_prefix("rqbit_two_ops")?;
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        content.write_to(&old);
        let session = new_session(dir.path(), Default::default()).await?;
        let handle = add(&session, &content, &old, Default::default()).await?;
        let a = content.file("a.bin").index;

        let (moved, renamed) = tokio::join!(
            session.move_storage(&handle, new.clone(), MoveStorageMode::FailIfExists),
            session.rename_file(&handle, a, "r/a.bin".into()),
        );
        moved?;
        renamed?;
        content.assert_at(&new, &[(a, "r/a.bin")])?;
        no_torrent_files_at(&content, &old)?;
        assert!(!old.join("r").exists());

        // A recheck and a move: the move stops the running check and starts it again.
        let (rechecked, moved) = tokio::join!(
            session.force_recheck(&handle),
            session.move_storage(&handle, old.clone(), MoveStorageMode::FailIfExists),
        );
        rechecked?;
        moved?;
        wait_initialized(&handle).await?;
        content.assert_at(&old, &[(a, "r/a.bin")])?;
        assert!(is_live(&handle));
        assert_eq!(have_pieces(&handle)?, content.all_pieces());
        Ok(())
    })
    .await
}

// Deleting a torrent while its storage moves: the move gives up and undoes itself, and the
// delete waits for that, so the files are all at the old place.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_move_storage_racing_delete() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let dir = TempDir::with_prefix("rqbit_move_delete")?;
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        content.write_to(&old);
        let session = new_session(dir.path(), Default::default()).await?;
        let ctl = Arc::new(Ctl::default());
        let handle = add(
            &session,
            &content,
            &old,
            AddTorrentOptions {
                storage_factory: Some(InstrumentedFactory { ctl: ctl.clone() }.boxed()),
                ..Default::default()
            },
        )
        .await?;

        ctl.move_gate.arm();
        let mv = tokio::spawn({
            let session = session.clone();
            let handle = handle.clone();
            let new = new.clone();
            async move {
                session
                    .move_storage(&handle, new, MoveStorageMode::FailIfExists)
                    .await
            }
        });
        ctl.move_gate.wait_entered().await?;
        let del = tokio::spawn({
            let session = session.clone();
            let id = handle.id();
            async move { session.delete(id.into(), false).await }
        });
        wait_for("the delete to cancel the move", || {
            handle.is_storage_op_cancelled()
        })
        .await?;
        ctl.move_gate.release();

        let moved = mv.await?;
        del.await??;
        assert!(moved.is_err(), "the move went through a delete");
        content.assert_at(&old, &[])?;
        no_torrent_files_at(&content, &new)?;
        assert!(session.get(handle.id().into()).is_none());
        Ok(())
    })
    .await
}

// Pause and unpause wait for a storage operation instead of changing the state under it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_operations_with_pause_and_unpause() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let dir = TempDir::with_prefix("rqbit_ops_pause")?;
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        content.write_to(&old);
        let session = new_session(dir.path(), Default::default()).await?;
        let ctl = Arc::new(Ctl::default());
        let handle = add(
            &session,
            &content,
            &old,
            AddTorrentOptions {
                storage_factory: Some(InstrumentedFactory { ctl: ctl.clone() }.boxed()),
                ..Default::default()
            },
        )
        .await?;

        ctl.move_gate.arm();
        let mv = tokio::spawn({
            let session = session.clone();
            let handle = handle.clone();
            let new = new.clone();
            async move {
                session
                    .move_storage(&handle, new, MoveStorageMode::FailIfExists)
                    .await
            }
        });
        ctl.move_gate.wait_entered().await?;
        let pause = tokio::spawn({
            let session = session.clone();
            let handle = handle.clone();
            async move { session.pause(&handle).await }
        });
        // The pause can't get in while the move runs.
        tokio::task::yield_now().await;
        assert!(!pause.is_finished(), "pause didn't wait for the move");
        ctl.move_gate.release();
        mv.await??;
        pause.await??;
        assert_eq!(handle.output_folder(), new);
        assert!(is_paused(&handle));
        assert!(handle.is_paused());
        content.assert_at(&new, &[])?;

        let (unpaused, rechecked) =
            tokio::join!(session.unpause(&handle), session.force_recheck(&handle));
        unpaused?;
        rechecked?;
        wait_initialized(&handle).await?;
        assert!(is_live(&handle), "{:?}", state(&handle));
        assert_eq!(have_pieces(&handle)?, content.all_pieces());
        Ok(())
    })
    .await
}
