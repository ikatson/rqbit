//! Web seeds (BEP 19) end to end: real sessions download from the in-process HTTP server of
//! [`super::web_seed_server`], alone or next to a real seeding session, while the server
//! misbehaves in the ways web servers do.

use std::{
    net::{Ipv4Addr, SocketAddr},
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use bytes::Bytes;
use sha1w::{ISha1, Sha1};
use tempfile::TempDir;
use tokio::time::timeout;

use crate::{
    AddTorrent, AddTorrentOptions, ListenerOptions, Session, SessionOptions,
    SessionPersistenceConfig, WebSeedState, WebSeedStats,
    api::TorrentIdOrHash,
    limits::LimitsConfig,
    tests::{test_util::setup_test_logging, web_seed_server::WebSeedServer},
    torrent_state::ManagedTorrentHandle,
};

pub(super) const PIECE: usize = 16 * 1024;
const KIB: u32 = 1024;
const WAIT: Duration = Duration::from_secs(30);

pub(super) async fn run(fut: impl Future<Output = anyhow::Result<()>>) -> anyhow::Result<()> {
    setup_test_logging();
    timeout(Duration::from_secs(90), fut)
        .await
        .context("test timed out")?
}

fn bps(v: u32) -> Option<NonZeroU32> {
    NonZeroU32::new(v)
}

/// Polls `cond` until it holds.
async fn until(what: &str, mut cond: impl FnMut() -> bool) -> anyhow::Result<()> {
    timeout(WAIT, async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .with_context(|| format!("timed out waiting until {what}"))
}

pub(super) async fn wait_completed(handle: &ManagedTorrentHandle) -> anyhow::Result<()> {
    timeout(WAIT, handle.wait_until_completed())
        .await
        .context("the torrent did not complete")?
}

pub(super) fn only_seed(handle: &ManagedTorrentHandle) -> anyhow::Result<WebSeedStats> {
    match &handle.web_seeds()[..] {
        [one] => Ok(one.clone()),
        other => bail!("expected one web seed, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// The torrents
// ---------------------------------------------------------------------------------------------

/// An entry of the file list of a multi-file torrent.
pub(super) enum Entry<'a> {
    File(&'a [&'a str], usize),
    /// A BEP 47 padding file.
    Pad(usize),
}

pub(super) struct Torrent {
    pub(super) bytes: Bytes,
    pub(super) name: String,
    pub(super) single: bool,
    /// The path components and content of every file that is not padding, in torrent order.
    pub(super) files: Vec<(Vec<String>, Vec<u8>)>,
    pub(super) pieces: u64,
}

impl Torrent {
    /// The decoded path of file `i` on the server, under the web seed URL of the server.
    pub(super) fn url_path(&self, i: usize) -> String {
        if self.single {
            format!("/seed/{}", self.name)
        } else {
            format!("/seed/{}/{}", self.name, self.files[i].0.join("/"))
        }
    }

    pub(super) fn server_files(&self) -> Vec<(String, Vec<u8>)> {
        (0..self.files.len())
            .map(|i| (self.url_path(i), self.files[i].1.clone()))
            .collect()
    }

    /// Where file `i` is when the torrent is downloaded to the output folder `out`.
    pub(super) fn disk_path(&self, out: &Path, i: usize) -> PathBuf {
        if self.single {
            out.join(&self.name)
        } else {
            self.files[i]
                .0
                .iter()
                .fold(out.to_owned(), |p, c| p.join(c))
        }
    }

    pub(super) fn content_len(&self) -> u64 {
        self.files.iter().map(|(_, d)| d.len() as u64).sum()
    }

    /// Asserts that the files in `out` are the torrent's, byte for byte.
    pub(super) fn check_files(&self, out: &Path) -> anyhow::Result<()> {
        for (i, (path, data)) in self.files.iter().enumerate() {
            let on_disk = std::fs::read(self.disk_path(out, i))
                .with_context(|| format!("reading {path:?}"))?;
            if on_disk != *data {
                bail!("{path:?} differs from the torrent's content");
            }
        }
        Ok(())
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

/// Different bytes in every piece of every file, so that misplaced data fails the hash check.
fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut x = 0x9e37_79b9_7f4a_7c15u64 ^ seed.wrapping_mul(0x2545_f491_4f6c_dd1d);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x.to_le_bytes()[0]
        })
        .collect()
}

pub(super) fn single_file(name: &str, len: usize, url_list: &[&str]) -> Torrent {
    build(name, true, &[Entry::File(&[], len)], url_list)
}

pub(super) fn multi_file(name: &str, entries: &[Entry<'_>], url_list: &[&str]) -> Torrent {
    build(name, false, entries, url_list)
}

/// A v1 torrent with [`PIECE`] long pieces. One `url_list` entry is written as a string, more
/// as a list.
fn build(name: &str, single: bool, entries: &[Entry<'_>], url_list: &[&str]) -> Torrent {
    let mut space = Vec::new();
    let mut files = Vec::new();
    let mut flist = b"l".to_vec();
    for (i, entry) in entries.iter().enumerate() {
        match *entry {
            Entry::File(path, len) => {
                let data = pseudo_random(len, i as u64 + name.len() as u64);
                space.extend_from_slice(&data);
                flist.push(b'd');
                flist.extend(bstr(b"length"));
                flist.extend(bint(len));
                flist.extend(bstr(b"path"));
                flist.push(b'l');
                for c in path {
                    flist.extend(bstr(c.as_bytes()));
                }
                flist.extend(b"ee");
                files.push((path.iter().map(|c| c.to_string()).collect(), data));
            }
            Entry::Pad(len) => {
                space.resize(space.len() + len, 0);
                flist.push(b'd');
                flist.extend(bstr(b"attr"));
                flist.extend(bstr(b"p"));
                flist.extend(bstr(b"length"));
                flist.extend(bint(len));
                flist.extend(bstr(b"path"));
                flist.push(b'l');
                flist.extend(bstr(b".pad"));
                flist.extend(bstr(len.to_string().as_bytes()));
                flist.extend(b"ee");
            }
        }
    }
    flist.push(b'e');

    let mut pieces = Vec::new();
    for chunk in space.chunks(PIECE) {
        let mut h = Sha1::new();
        h.update(chunk);
        pieces.extend_from_slice(&h.finish());
    }

    let mut info = b"d".to_vec();
    if single {
        info.extend(bstr(b"length"));
        info.extend(bint(space.len()));
    } else {
        info.extend(bstr(b"files"));
        info.extend(flist);
    }
    info.extend(bstr(b"name"));
    info.extend(bstr(name.as_bytes()));
    info.extend(bstr(b"piece length"));
    info.extend(bint(PIECE));
    info.extend(bstr(b"pieces"));
    info.extend(bstr(&pieces));
    info.push(b'e');

    let mut t = b"d".to_vec();
    t.extend(bstr(b"info"));
    t.extend(info);
    match url_list {
        [] => {}
        [one] => {
            t.extend(bstr(b"url-list"));
            t.extend(bstr(one.as_bytes()));
        }
        many => {
            t.extend(bstr(b"url-list"));
            t.push(b'l');
            for url in many {
                t.extend(bstr(url.as_bytes()));
            }
            t.push(b'e');
        }
    }
    t.push(b'e');

    Torrent {
        bytes: t.into(),
        name: name.to_owned(),
        single,
        files,
        pieces: space.len().div_ceil(PIECE) as u64,
    }
}

// ---------------------------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------------------------

fn client_opts() -> SessionOptions {
    SessionOptions {
        dht: None,
        persistence: None,
        listen: None,
        ipv4_only: true,
        disable_local_service_discovery: true,
        ..Default::default()
    }
}

pub(super) struct Client {
    pub(super) session: Arc<Session>,
    pub(super) handle: ManagedTorrentHandle,
    pub(super) out: PathBuf,
    _dir: TempDir,
}

impl Client {
    pub(super) async fn start(torrent: &Torrent, opts: AddTorrentOptions) -> anyhow::Result<Self> {
        Self::start_with(torrent, client_opts(), opts).await
    }

    async fn start_with(
        torrent: &Torrent,
        session_opts: SessionOptions,
        opts: AddTorrentOptions,
    ) -> anyhow::Result<Self> {
        let dir = TempDir::with_prefix("rqbit_webseed_client")?;
        let session = Session::new_with_opts(dir.path().into(), session_opts)
            .await
            .context("error creating client session")?;
        let out = dir.path().join("out");
        let handle = add(&session, torrent, &out, opts).await?;
        Ok(Self {
            session,
            handle,
            out,
            _dir: dir,
        })
    }

    pub(super) fn checked_pieces(&self) -> u64 {
        self.handle
            .live()
            .map_or(0, |l| l.stats_snapshot().downloaded_and_checked_pieces)
    }
}

async fn add(
    session: &Arc<Session>,
    torrent: &Torrent,
    out: &Path,
    opts: AddTorrentOptions,
) -> anyhow::Result<ManagedTorrentHandle> {
    session
        .add_torrent(
            AddTorrent::from_bytes(torrent.bytes.clone()),
            Some(AddTorrentOptions {
                output_folder: Some(out.to_str().context("path")?.to_owned()),
                ..opts
            }),
        )
        .await?
        .into_handle()
        .context("into_handle")
}

/// A session seeding the torrent on loopback.
struct Seeder {
    session: Arc<Session>,
    handle: ManagedTorrentHandle,
    addr: SocketAddr,
    _dir: TempDir,
}

impl Seeder {
    async fn start(torrent: &Torrent, upload_bps: Option<NonZeroU32>) -> anyhow::Result<Self> {
        let dir = TempDir::with_prefix("rqbit_webseed_seeder")?;
        let out = dir.path().join("out");
        for i in 0..torrent.files.len() {
            let path = torrent.disk_path(&out, i);
            std::fs::create_dir_all(path.parent().context("parent")?)?;
            std::fs::write(&path, &torrent.files[i].1)?;
        }
        let session = Session::new_with_opts(
            dir.path().into(),
            SessionOptions {
                listen: Some(ListenerOptions {
                    listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
                    ipv4_only: true,
                    ..Default::default()
                }),
                ..client_opts()
            },
        )
        .await
        .context("error creating seeder session")?;
        let handle = add(
            &session,
            torrent,
            &out,
            AddTorrentOptions {
                overwrite: true,
                disable_web_seeds: true,
                ratelimits: LimitsConfig {
                    upload_bps,
                    download_bps: None,
                },
                ..Default::default()
            },
        )
        .await?;
        wait_completed(&handle).await?;
        Ok(Self {
            addr: session.listen_addr().context("seeder listens")?,
            session,
            handle,
            _dir: dir,
        })
    }

    fn uploaded(&self) -> u64 {
        self.handle.live().map_or(0, |l| l.get_uploaded_bytes())
    }
}

// At `rate` bytes per second, moving `bytes` takes at least this long: the bucket starts with
// one second worth of the rate, and a few blocks may already be on their way.
fn at_least(bytes: u64, rate: u32) -> Duration {
    let free = u64::from(rate) + 64 * 1024;
    Duration::from_secs_f64(bytes.saturating_sub(free) as f64 / f64::from(rate) * 0.8)
}

/// A server for the torrent that `build` makes from a `url-list`, and that torrent, whose
/// `url-list` is the server's URL followed by `more`. The content does not depend on the list.
async fn serve(
    build: impl Fn(&[&str]) -> Torrent,
    more: &[&str],
) -> anyhow::Result<(WebSeedServer, Torrent)> {
    let server = WebSeedServer::start(build(&[]).server_files()).await?;
    let url = server.url();
    let list: Vec<&str> = std::iter::once(url.as_str())
        .chain(more.iter().copied())
        .collect();
    let torrent = build(&list);
    Ok((server, torrent))
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_web_seed_alone_single_file() -> anyhow::Result<()> {
    run(async {
        let (server, torrent) = serve(
            |l| single_file("single file.bin", 20 * PIECE + 1234, l),
            &[],
        )
        .await?;
        let client = Client::start(&torrent, AddTorrentOptions::default()).await?;
        wait_completed(&client.handle).await?;
        torrent.check_files(&client.out)?;

        let seed = only_seed(&client.handle)?;
        assert_eq!(seed.url, server.url());
        assert_eq!(seed.downloaded_bytes, torrent.content_len());
        assert_eq!(seed.last_error, None);
        assert_ne!(seed.state, WebSeedState::Disabled);
        assert_eq!(client.checked_pieces(), torrent.pieces);
        // Every byte was asked for once.
        assert_eq!(server.body_bytes(), torrent.content_len());
        let log = server.log();
        assert!(!log.is_empty());
        for s in &log {
            assert_eq!(s.path, "/seed/single file.bin");
            assert_eq!(s.status, 206, "{s:?}");
            assert!(s.range.is_some(), "{s:?}");
        }
        client.session.stop().await;
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_web_seed_alone_multi_file_nested_and_padded() -> anyhow::Result<()> {
    run(async {
        let (server, torrent) = serve(
            |l| {
                multi_file(
                    "web seed",
                    &[
                        Entry::File(&["a b.bin"], 3 * PIECE - 100),
                        Entry::Pad(100),
                        Entry::File(&["dir one", "inner", "100%.txt"], 2 * PIECE + 5),
                        Entry::Pad(PIECE - 5),
                        Entry::File(&["dir one", "c"], 1000),
                    ],
                    l,
                )
            },
            // Garbage in the list is skipped.
            &["ftp://example.com/x", "not a url"],
        )
        .await?;
        let url = server.url();
        let client = Client::start(&torrent, AddTorrentOptions::default()).await?;
        wait_completed(&client.handle).await?;
        torrent.check_files(&client.out)?;
        assert!(
            !client.out.join(".pad").exists(),
            "a padding file was written"
        );

        let seed = only_seed(&client.handle)?;
        assert_eq!(seed.url, url);
        assert_eq!(seed.downloaded_bytes, torrent.content_len());
        assert_eq!(client.checked_pieces(), torrent.pieces);
        // The zeros of the padding file never crossed the network and do not count.
        let fetched = client
            .handle
            .live()
            .context("not live")?
            .stats_snapshot()
            .fetched_bytes;
        assert_eq!(fetched, torrent.content_len());
        assert_eq!(server.body_bytes(), torrent.content_len());
        for s in server.log() {
            assert!(
                (0..torrent.files.len()).any(|i| s.path == torrent.url_path(i)),
                "unexpected request {s:?}"
            );
            assert_eq!(s.status, 206, "{s:?}");
        }
        for i in 0..torrent.files.len() {
            assert!(
                server.served(&torrent.url_path(i), 206) > 0,
                "nothing asked from {}",
                torrent.url_path(i)
            );
        }
        client.session.stop().await;
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_web_seed_and_peer_together() -> anyhow::Result<()> {
    run(async {
        let torrent = single_file("mixed.bin", 32 * PIECE, &[]);
        // Both are slow, so neither can do it all before the other starts.
        let seeder = Seeder::start(&torrent, bps(48 * KIB)).await?;
        let server = WebSeedServer::start(torrent.server_files()).await?;
        server.set(|b| b.drip = Some((PIECE, Duration::from_millis(100))));
        let client = Client::start(
            &torrent,
            AddTorrentOptions {
                initial_peers: Some(vec![seeder.addr]),
                web_seeds: Some(vec![server.url()]),
                ..Default::default()
            },
        )
        .await?;
        wait_completed(&client.handle).await?;
        torrent.check_files(&client.out)?;

        let seed = only_seed(&client.handle)?;
        assert!(seed.downloaded_bytes > 0, "the web seed gave nothing");
        assert!(seeder.uploaded() > 0, "the peer gave nothing");
        // No piece was completed twice, none failed.
        assert_eq!(client.checked_pieces(), torrent.pieces);
        assert_eq!(seed.last_error, None);
        // Whole pieces only went to one of them, and the web seed only delivered what was
        // written: together they cover the torrent.
        assert!(seed.downloaded_bytes + seeder.uploaded() >= torrent.content_len());
        client.session.stop().await;
        seeder.session.stop().await;
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_corrupt_web_seed_is_disabled_and_the_peer_fills_in() -> anyhow::Result<()> {
    run(async {
        let torrent = single_file("corrupt.bin", 16 * PIECE, &[]);
        let seeder = Seeder::start(&torrent, bps(64 * KIB)).await?;
        let server = WebSeedServer::start(torrent.server_files()).await?;
        let path = torrent.url_path(0);
        server.set(|b| b.corrupt = vec![(path.clone(), 0..torrent.content_len())]);
        let client = Client::start(
            &torrent,
            AddTorrentOptions {
                initial_peers: Some(vec![seeder.addr]),
                web_seeds: Some(vec![server.url()]),
                ..Default::default()
            },
        )
        .await?;
        until("the web seed is disabled", || {
            only_seed(&client.handle).is_ok_and(|s| s.state == WebSeedState::Disabled)
        })
        .await?;
        wait_completed(&client.handle).await?;
        torrent.check_files(&client.out)?;

        let seed = only_seed(&client.handle)?;
        assert_eq!(seed.state, WebSeedState::Disabled);
        let error = seed.last_error.context("no error")?;
        assert!(error.contains("hash check"), "{error}");
        assert_eq!(client.checked_pieces(), torrent.pieces);
        assert!(seeder.uploaded() >= torrent.content_len());
        client.session.stop().await;
        seeder.session.stop().await;
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_server_without_ranges_is_disabled() -> anyhow::Result<()> {
    run(async {
        // More pieces than a web seed reserves at once: some request starts past offset 0.
        let torrent = single_file("whole.bin", 20 * PIECE, &[]);
        let server = WebSeedServer::start(torrent.server_files()).await?;
        server.set(|b| b.ignore_range = true);
        let client = Client::start(
            &torrent,
            AddTorrentOptions {
                web_seeds: Some(vec![server.url()]),
                ..Default::default()
            },
        )
        .await?;
        until("the web seed is disabled", || {
            only_seed(&client.handle).is_ok_and(|s| s.state == WebSeedState::Disabled)
        })
        .await?;
        let seed = only_seed(&client.handle)?;
        let error = seed.last_error.context("no error")?;
        assert!(error.contains("range"), "{error}");
        assert!(!client.handle.stats().finished);
        client.session.stop().await;
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_wrong_length_disables_the_web_seed() -> anyhow::Result<()> {
    run(async {
        let torrent = single_file("short.bin", 8 * PIECE, &[]);
        let server = WebSeedServer::start(torrent.server_files()).await?;
        server.set(|b| b.wrong_length = true);
        let client = Client::start(
            &torrent,
            AddTorrentOptions {
                web_seeds: Some(vec![server.url()]),
                ..Default::default()
            },
        )
        .await?;
        until("the web seed is disabled", || {
            only_seed(&client.handle).is_ok_and(|s| s.state == WebSeedState::Disabled)
        })
        .await?;
        let seed = only_seed(&client.handle)?;
        assert_eq!(seed.downloaded_bytes, 0);
        assert_eq!(client.checked_pieces(), 0);
        // Not retried.
        assert_eq!(server.log().len(), 1);
        client.session.stop().await;
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_server_without_ranges_serves_a_whole_file_from_offset_0() -> anyhow::Result<()> {
    run(async {
        // One piece: the only request is for the whole file.
        let torrent = single_file("small.bin", 10_000, &[]);
        let server = WebSeedServer::start(torrent.server_files()).await?;
        server.set(|b| b.ignore_range = true);
        let client = Client::start(
            &torrent,
            AddTorrentOptions {
                web_seeds: Some(vec![server.url()]),
                ..Default::default()
            },
        )
        .await?;
        wait_completed(&client.handle).await?;
        torrent.check_files(&client.out)?;
        let seed = only_seed(&client.handle)?;
        assert_ne!(seed.state, WebSeedState::Disabled);
        assert_eq!(seed.last_error, None);
        assert_eq!(server.served(&torrent.url_path(0), 200), 1);
        client.session.stop().await;
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_unavailable_server_backs_off_then_recovers() -> anyhow::Result<()> {
    run(async {
        let torrent = single_file("busy.bin", 8 * PIECE, &[]);
        let server = WebSeedServer::start(torrent.server_files()).await?;
        // Answered with 503 and "Retry-After: 1".
        server.set(|b| b.unavailable = 2);
        let started = Instant::now();
        let client = Client::start(
            &torrent,
            AddTorrentOptions {
                web_seeds: Some(vec![server.url()]),
                ..Default::default()
            },
        )
        .await?;
        until("the web seed backs off", || {
            only_seed(&client.handle).is_ok_and(|s| s.state == WebSeedState::BackingOff)
        })
        .await?;
        wait_completed(&client.handle).await?;
        torrent.check_files(&client.out)?;
        // Retry-After was honoured twice.
        assert!(started.elapsed() >= Duration::from_secs(2));
        let seed = only_seed(&client.handle)?;
        assert_ne!(seed.state, WebSeedState::Disabled);
        assert!(
            seed.last_error
                .as_deref()
                .is_some_and(|e| e.contains("503")),
            "{seed:?}"
        );
        assert_eq!(server.served(&torrent.url_path(0), 503), 2);
        client.session.stop().await;
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_missing_file_comes_from_the_peer() -> anyhow::Result<()> {
    run(async {
        let torrent = multi_file(
            "missing",
            &[
                Entry::File(&["a"], 8 * PIECE),
                Entry::File(&["b"], 2 * PIECE),
                Entry::File(&["c"], 8 * PIECE),
            ],
            &[],
        );
        let seeder = Seeder::start(&torrent, bps(32 * KIB)).await?;
        let server = WebSeedServer::start(torrent.server_files()).await?;
        let missing = torrent.url_path(1);
        server.set(|b| b.missing = vec![missing.clone()]);
        let client = Client::start(
            &torrent,
            AddTorrentOptions {
                initial_peers: Some(vec![seeder.addr]),
                web_seeds: Some(vec![server.url()]),
                ..Default::default()
            },
        )
        .await?;
        wait_completed(&client.handle).await?;
        torrent.check_files(&client.out)?;

        let seed = only_seed(&client.handle)?;
        assert_ne!(seed.state, WebSeedState::Disabled, "{seed:?}");
        assert!(server.served(&torrent.url_path(0), 206) > 0);
        assert!(server.served(&torrent.url_path(2), 206) > 0);
        assert!(server.served(&missing, 206) == 0);
        // Once known missing, the file is not asked for again.
        assert_eq!(server.served(&missing, 404), 1);
        assert!(seeder.uploaded() >= 2 * PIECE as u64);
        client.session.stop().await;
        seeder.session.stop().await;
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pause_cancels_requests_and_resume_continues() -> anyhow::Result<()> {
    run(async {
        let torrent = single_file("pause.bin", 8 * PIECE, &[]);
        let server = WebSeedServer::start(torrent.server_files()).await?;
        server.set(|b| b.stall = true);
        let client = Client::start(
            &torrent,
            AddTorrentOptions {
                web_seeds: Some(vec![server.url()]),
                ..Default::default()
            },
        )
        .await?;
        until("a request is pending", || server.in_flight() == 1).await?;
        client.session.pause(&client.handle).await?;
        until("the request is cancelled", || {
            server.aborted() == 1 && server.in_flight() == 0
        })
        .await?;
        assert_eq!(only_seed(&client.handle)?.state, WebSeedState::Idle);

        server.set(|b| b.stall = false);
        client.session.unpause(&client.handle).await?;
        wait_completed(&client.handle).await?;
        torrent.check_files(&client.out)?;
        client.session.stop().await;
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_delete_cancels_requests() -> anyhow::Result<()> {
    run(async {
        let torrent = single_file("delete.bin", 8 * PIECE, &[]);
        let server = WebSeedServer::start(torrent.server_files()).await?;
        server.set(|b| b.stall = true);
        let client = Client::start(
            &torrent,
            AddTorrentOptions {
                web_seeds: Some(vec![server.url()]),
                ..Default::default()
            },
        )
        .await?;
        until("a request is pending", || server.in_flight() == 1).await?;
        client
            .session
            .delete(TorrentIdOrHash::Id(client.handle.id()), true)
            .await?;
        until("the request is cancelled", || {
            server.aborted() == 1 && server.in_flight() == 0
        })
        .await?;
        client.session.stop().await;
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_torrent_download_limit_applies_to_web_seeds() -> anyhow::Result<()> {
    run(async {
        let len = 32 * PIECE;
        let rate = 64 * KIB;
        let torrent = single_file("limited.bin", len, &[]);
        let server = WebSeedServer::start(torrent.server_files()).await?;
        let started = Instant::now();
        let client = Client::start(
            &torrent,
            AddTorrentOptions {
                web_seeds: Some(vec![server.url()]),
                ratelimits: LimitsConfig {
                    upload_bps: None,
                    download_bps: bps(rate),
                },
                ..Default::default()
            },
        )
        .await?;
        // A torrent that only has a web seed is not stalled: its speed shows.
        until("the download speed shows", || {
            client
                .handle
                .live()
                .is_some_and(|l| l.down_speed_estimator().mbps() > 0.0)
        })
        .await?;
        wait_completed(&client.handle).await?;
        let took = started.elapsed();
        assert!(
            took >= at_least(len as u64, rate),
            "{len} bytes in {took:?} at {rate} B/s"
        );
        torrent.check_files(&client.out)?;
        client.session.stop().await;
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_session_download_limit_applies_to_web_seeds() -> anyhow::Result<()> {
    run(async {
        let len = 32 * PIECE;
        let rate = 64 * KIB;
        let torrent = single_file("session-limited.bin", len, &[]);
        let server = WebSeedServer::start(torrent.server_files()).await?;
        let started = Instant::now();
        let client = Client::start_with(
            &torrent,
            SessionOptions {
                ratelimits: LimitsConfig {
                    upload_bps: None,
                    download_bps: bps(rate),
                },
                ..client_opts()
            },
            AddTorrentOptions {
                web_seeds: Some(vec![server.url()]),
                ..Default::default()
            },
        )
        .await?;
        wait_completed(&client.handle).await?;
        let took = started.elapsed();
        assert!(
            took >= at_least(len as u64, rate),
            "{len} bytes in {took:?} at {rate} B/s"
        );
        torrent.check_files(&client.out)?;
        client.session.stop().await;
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_add_and_remove_web_seeds_at_run_time() -> anyhow::Result<()> {
    run(async {
        let torrent = single_file("runtime.bin", 8 * PIECE, &[]);
        let stalled = WebSeedServer::start(torrent.server_files()).await?;
        stalled.set(|b| b.stall = true);
        let good = WebSeedServer::start(torrent.server_files()).await?;
        let client = Client::start(&torrent, AddTorrentOptions::default()).await?;
        let session = &client.session;
        let handle = &client.handle;
        assert!(handle.web_seeds().is_empty());

        session.add_web_seed(handle, &stalled.url()).await?;
        // Adding it again does nothing.
        session.add_web_seed(handle, &stalled.url()).await?;
        assert_eq!(handle.web_seeds().len(), 1);
        assert!(
            session
                .add_web_seed(handle, "ftp://example.com/")
                .await
                .is_err()
        );
        until("a request is pending", || stalled.in_flight() == 1).await?;

        session.remove_web_seed(handle, &stalled.url()).await?;
        until("the request is cancelled", || {
            stalled.aborted() == 1 && stalled.in_flight() == 0
        })
        .await?;
        assert!(handle.web_seeds().is_empty());
        assert!(
            session
                .remove_web_seed(handle, &stalled.url())
                .await
                .is_err()
        );

        session.add_web_seed(handle, &good.url()).await?;
        wait_completed(handle).await?;
        torrent.check_files(&client.out)?;
        assert_eq!(only_seed(handle)?.url, good.url());
        session.stop().await;
        Ok(())
    })
    .await
}

/// Web seeds added and removed at run time are stored by `persistence` (given the session's
/// folder) and come back after a restart.
async fn check_restart(
    persistence: impl Fn(&Path) -> Option<SessionPersistenceConfig>,
) -> anyhow::Result<()> {
    let dead = "http://127.0.0.1:1/dead/";
    let torrent = single_file("persist.bin", 8 * PIECE, &[dead]);
    let server = WebSeedServer::start(torrent.server_files()).await?;
    let dir = TempDir::with_prefix("rqbit_webseed_persist")?;
    let out = dir.path().join("out");
    let persistence = || persistence(dir.path());
    let session = Session::new_with_opts(
        dir.path().into(),
        SessionOptions {
            persistence: persistence(),
            ..client_opts()
        },
    )
    .await?;
    let handle = add(
        &session,
        &torrent,
        &out,
        AddTorrentOptions {
            paused: true,
            ..Default::default()
        },
    )
    .await?;
    assert_eq!(only_seed(&handle)?.url, dead);
    let info_hash = handle.info_hash();
    session.add_web_seed(&handle, &server.url()).await?;
    session.remove_web_seed(&handle, dead).await?;
    session.stop().await;
    drop(handle);
    drop(session);

    let session = Session::new_with_opts(
        dir.path().into(),
        SessionOptions {
            persistence: persistence(),
            ..client_opts()
        },
    )
    .await?;
    let handle = session
        .get(TorrentIdOrHash::Hash(info_hash))
        .context("torrent was not restored")?;
    // The removed web seed of the torrent's own url-list stays removed.
    assert_eq!(only_seed(&handle)?.url, server.url());
    handle.wait_until_initialized().await?;
    session.unpause(&handle).await?;
    wait_completed(&handle).await?;
    torrent.check_files(&out)?;
    // Leaves the store as it found it (the postgres database is shared by the checks).
    session
        .delete(TorrentIdOrHash::Hash(info_hash), false)
        .await?;
    session.stop().await;
    Ok(())
}

/// A torrent added with its web seeds disabled keeps them disabled after a restart through
/// `persistence`: the server is never asked.
async fn check_disabled_restart(
    persistence: impl Fn(&Path) -> Option<SessionPersistenceConfig>,
) -> anyhow::Result<()> {
    let (server, torrent) = serve(|l| single_file("off.bin", 8 * PIECE, l), &[]).await?;
    let dir = TempDir::with_prefix("rqbit_webseed_off")?;
    let out = dir.path().join("out");
    let opts = || SessionOptions {
        persistence: persistence(dir.path()),
        ..client_opts()
    };
    let session = Session::new_with_opts(dir.path().into(), opts()).await?;
    let handle = add(
        &session,
        &torrent,
        &out,
        AddTorrentOptions {
            disable_web_seeds: true,
            ..Default::default()
        },
    )
    .await?;
    let info_hash = handle.info_hash();
    assert_eq!(only_seed(&handle)?.state, WebSeedState::Disabled);
    session.stop().await;
    drop(handle);
    drop(session);

    let session = Session::new_with_opts(dir.path().into(), opts()).await?;
    let handle = session
        .get(TorrentIdOrHash::Hash(info_hash))
        .context("torrent was not restored")?;
    handle.wait_until_initialized().await?;
    let seed = only_seed(&handle)?;
    assert_eq!(seed.url, server.url());
    assert_eq!(seed.state, WebSeedState::Disabled);
    // A live torrent with an enabled web seed would have asked it by now: the first request
    // goes out as soon as the torrent is live.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(server.log().is_empty(), "{:?}", server.log());
    session
        .delete(TorrentIdOrHash::Hash(info_hash), false)
        .await?;
    session.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_disabled_web_seeds_survive_a_session_restart() -> anyhow::Result<()> {
    run(check_disabled_restart(|dir| {
        Some(SessionPersistenceConfig::Json {
            folder: Some(dir.join("session")),
        })
    }))
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_web_seeds_survive_a_session_restart() -> anyhow::Result<()> {
    run(check_restart(|dir| {
        Some(SessionPersistenceConfig::Json {
            folder: Some(dir.join("session")),
        })
    }))
    .await
}

/// The same through the Postgres store, one check after the other. Needs a database of its
/// own: `RQBIT_TEST_POSTGRES_URL=postgresql://... cargo test -p librqbit --features postgres
/// -- --ignored postgres`
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RQBIT_TEST_POSTGRES_URL"]
async fn test_web_seeds_survive_a_session_restart_postgres() -> anyhow::Result<()> {
    let url = std::env::var("RQBIT_TEST_POSTGRES_URL")
        .context("RQBIT_TEST_POSTGRES_URL must point at a database of its own")?;
    let persistence = move |_: &Path| {
        Some(SessionPersistenceConfig::Postgres {
            connection_string: url.clone(),
        })
    };
    run(async {
        check_restart(&persistence).await?;
        check_disabled_restart(&persistence).await
    })
    .await
}

/// A seeder and a web seed are both there, web seeds are disabled: everything comes from the
/// seeder and the server is never asked.
async fn check_disabled(
    session_opts: SessionOptions,
    opts: AddTorrentOptions,
) -> anyhow::Result<()> {
    let (server, torrent) = serve(|l| single_file("disabled.bin", 8 * PIECE, l), &[]).await?;
    let seeder = Seeder::start(&torrent, None).await?;
    let client = Client::start_with(
        &torrent,
        session_opts,
        AddTorrentOptions {
            initial_peers: Some(vec![seeder.addr]),
            ..opts
        },
    )
    .await?;
    wait_completed(&client.handle).await?;
    torrent.check_files(&client.out)?;
    let seed = only_seed(&client.handle)?;
    assert_eq!(seed.url, server.url());
    assert_eq!(seed.state, WebSeedState::Disabled);
    assert_eq!(seed.downloaded_bytes, 0);
    assert!(server.log().is_empty(), "{:?}", server.log());
    client.session.stop().await;
    seeder.session.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_web_seeds_disabled_for_the_session() -> anyhow::Result<()> {
    run(check_disabled(
        SessionOptions {
            disable_web_seeds: true,
            ..client_opts()
        },
        AddTorrentOptions::default(),
    ))
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_web_seeds_disabled_for_the_torrent() -> anyhow::Result<()> {
    run(check_disabled(
        client_opts(),
        AddTorrentOptions {
            disable_web_seeds: true,
            ..Default::default()
        },
    ))
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_skipped_file_is_never_requested() -> anyhow::Result<()> {
    run(async {
        // Files are piece aligned: no piece of a wanted file holds bytes of the skipped one.
        let torrent = multi_file(
            "skip",
            &[
                Entry::File(&["a"], 4 * PIECE),
                Entry::File(&["b"], 4 * PIECE),
                Entry::File(&["c"], 4 * PIECE),
            ],
            &[],
        );
        let server = WebSeedServer::start(torrent.server_files()).await?;
        let client = Client::start(
            &torrent,
            AddTorrentOptions {
                web_seeds: Some(vec![server.url()]),
                only_files: Some(vec![0, 2]),
                ..Default::default()
            },
        )
        .await?;
        wait_completed(&client.handle).await?;
        for i in [0, 2] {
            let on_disk = std::fs::read(torrent.disk_path(&client.out, i))?;
            assert_eq!(on_disk, torrent.files[i].1);
        }
        let skipped = torrent.url_path(1);
        assert!(
            server.log().iter().all(|s| s.path != skipped),
            "the skipped file was requested: {:?}",
            server.log()
        );
        client.session.stop().await;
        Ok(())
    })
    .await
}
