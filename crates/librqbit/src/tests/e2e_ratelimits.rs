//! Per-torrent rate limits changed while the torrent runs, between two sessions on loopback.
//!
//! Rates are only checked from one side: a slow machine can make a transfer slower, never
//! faster than the limit, so the tests assert how long a transfer took at least, and that a
//! lifted limit lets the rest through in a time the old limit could not have allowed.

use std::{
    net::{Ipv4Addr, SocketAddr},
    num::NonZeroU32,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;
use bytes::Bytes;
use librqbit_core::Id20;
use tempfile::TempDir;
use tokio::time::timeout;

use crate::{
    AddTorrent, AddTorrentOptions, ConnectionOptions, CreateTorrentOptions, ListenerOptions,
    Session, SessionOptions, SessionPersistenceConfig,
    api::TorrentIdOrHash,
    create_torrent,
    limits::LimitsConfig,
    spawn_utils::BlockingSpawner,
    tests::test_util::{create_default_random_dir_with_torrents, setup_test_logging, wait_until},
    torrent_state::ManagedTorrentHandle,
};

const PIECE_LEN: u32 = 64 * 1024;
const TORRENT_LEN: usize = 4 * 1024 * 1024;
const KIB: u32 = 1024;

async fn run(fut: impl Future<Output = anyhow::Result<()>>) -> anyhow::Result<()> {
    setup_test_logging();
    timeout(Duration::from_secs(90), fut)
        .await
        .context("test timed out")?
}

fn bps(v: u32) -> Option<NonZeroU32> {
    NonZeroU32::new(v)
}

fn loopback_listener() -> ListenerOptions {
    ListenerOptions {
        listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
        ipv4_only: true,
        ..Default::default()
    }
}

struct Seeder {
    session: Arc<Session>,
    handle: ManagedTorrentHandle,
    addr: SocketAddr,
    torrent_bytes: Bytes,
    info_hash: Id20,
    _dir: TempDir,
}

impl Seeder {
    fn uploaded(&self) -> u64 {
        self.handle.live().map_or(0, |l| l.get_uploaded_bytes())
    }
}

async fn start_seeder() -> anyhow::Result<Seeder> {
    let dir = create_default_random_dir_with_torrents(1, TORRENT_LEN, Some("rqbit_ratelimit_seed"));
    let torrent = create_torrent(
        dir.path(),
        CreateTorrentOptions {
            name: None,
            piece_length: Some(PIECE_LEN),
            ..Default::default()
        },
        &BlockingSpawner::new(1),
    )
    .await?;
    let torrent_bytes = torrent.as_bytes()?;
    let session = Session::new_with_opts(
        dir.path().into(),
        SessionOptions {
            dht: None,
            persistence: None,
            listen: Some(loopback_listener()),
            ipv4_only: true,
            disable_local_service_discovery: true,
            ..Default::default()
        },
    )
    .await
    .context("error creating seeder session")?;
    let handle = session
        .add_torrent(
            AddTorrent::from_bytes(torrent_bytes.clone()),
            Some(AddTorrentOptions {
                output_folder: Some(dir.path().to_str().context("path")?.to_owned()),
                overwrite: true,
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .context("into_handle")?;
    timeout(Duration::from_secs(30), handle.wait_until_completed())
        .await
        .context("seeder did not become a seeder")??;
    Ok(Seeder {
        addr: session.listen_addr().context("seeder listens")?,
        session,
        handle,
        torrent_bytes,
        info_hash: torrent.info_hash(),
        _dir: dir,
    })
}

struct Client {
    session: Arc<Session>,
    handle: ManagedTorrentHandle,
    _dir: TempDir,
}

impl Client {
    // Fetched by the current live state (the counter starts over after a resume).
    fn fetched(&self) -> u64 {
        self.handle
            .live()
            .map_or(0, |l| l.stats_snapshot().fetched_bytes)
    }
}

async fn start_client(seed: &Seeder, opts: AddTorrentOptions) -> anyhow::Result<Client> {
    let dir = TempDir::with_prefix("rqbit_ratelimit_client")?;
    let session = Session::new_with_opts(
        dir.path().into(),
        SessionOptions {
            dht: None,
            persistence: None,
            listen: None,
            connect: Some(ConnectionOptions::default()),
            ipv4_only: true,
            disable_local_service_discovery: true,
            ..Default::default()
        },
    )
    .await
    .context("error creating client session")?;
    let handle = session
        .add_torrent(
            AddTorrent::from_bytes(seed.torrent_bytes.clone()),
            Some(AddTorrentOptions {
                initial_peers: Some(vec![seed.addr]),
                ..opts
            }),
        )
        .await?
        .into_handle()
        .context("into_handle")?;
    handle.wait_until_initialized().await?;
    Ok(Client {
        session,
        handle,
        _dir: dir,
    })
}

async fn wait_for(
    what: &str,
    mut cond: impl FnMut() -> bool,
    within: Duration,
) -> anyhow::Result<()> {
    wait_until(
        || {
            if cond() {
                Ok(())
            } else {
                anyhow::bail!("{what}")
            }
        },
        within,
    )
    .await
}

// How long it takes `counter` to grow by `bytes`.
async fn time_to_move(
    what: &str,
    counter: impl Fn() -> u64,
    bytes: u64,
) -> anyhow::Result<Duration> {
    let start = Instant::now();
    let from = counter();
    wait_for(what, || counter() >= from + bytes, Duration::from_secs(30)).await?;
    Ok(start.elapsed())
}

// At `rate` bytes per second, moving `bytes` takes at least this long: the bucket starts with
// one second worth of the rate, and a few blocks may already have been on their way (a download
// limit is taken before a request goes out, so what was asked for earlier still arrives).
fn at_least(bytes: u64, rate: u32) -> Duration {
    let free = u64::from(rate) + 64 * 1024;
    Duration::from_secs_f64(bytes.saturating_sub(free) as f64 / f64::from(rate) * 0.8)
}

// The old limit is 1 KiB/s: a block parked on it needs 15 more seconds, the rest of the
// torrent hours. Finishing within this shows that both the parked transfer and the next ones
// follow the lifted limit.
const LIFTED_WITHIN: Duration = Duration::from_secs(12);

#[tokio::test(flavor = "multi_thread")]
async fn test_torrent_upload_limit_changes_while_seeding() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder().await?;
        let fast = LimitsConfig {
            upload_bps: bps(512 * KIB),
            download_bps: None,
        };
        seed.session
            .set_torrent_ratelimits(&seed.handle, fast)
            .await?;
        assert_eq!(seed.handle.ratelimits(), fast);

        let client = start_client(&seed, AddTorrentOptions::default()).await?;
        wait_for(
            "the transfer to start",
            || seed.uploaded() >= 256 * 1024,
            Duration::from_secs(30),
        )
        .await?;

        // Lowered mid-transfer: the rate drops to it.
        let slow = LimitsConfig {
            upload_bps: bps(32 * KIB),
            download_bps: None,
        };
        seed.session
            .set_torrent_ratelimits(&seed.handle, slow)
            .await?;
        assert_eq!(seed.handle.ratelimits(), slow);
        let bytes = 192 * 1024;
        let took = time_to_move("uploads at the lowered rate", || seed.uploaded(), bytes).await?;
        assert!(
            took >= at_least(bytes, 32 * KIB),
            "{bytes} bytes in {took:?} at 32 KiB/s"
        );

        // Nearly stopped, then lifted: what waits for the old rate goes on at once.
        seed.session
            .set_torrent_ratelimits(
                &seed.handle,
                LimitsConfig {
                    upload_bps: bps(KIB),
                    download_bps: None,
                },
            )
            .await?;
        tokio::time::sleep(Duration::from_secs(1)).await;
        seed.session
            .set_torrent_ratelimits(&seed.handle, LimitsConfig::default())
            .await?;
        assert_eq!(seed.handle.ratelimits(), LimitsConfig::default());
        timeout(LIFTED_WITHIN, client.handle.wait_until_completed())
            .await
            .context("the transfer kept the old limit")??;
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_torrent_download_limit_changes_while_downloading() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder().await?;
        // Low enough that the initial burst is long gone, and with it the requests sent under
        // it, by the time the limit is lowered.
        let fast = LimitsConfig {
            upload_bps: None,
            download_bps: bps(128 * KIB),
        };
        let client = start_client(
            &seed,
            AddTorrentOptions {
                ratelimits: fast,
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(client.handle.ratelimits(), fast);
        wait_for(
            "the transfer to start",
            || client.fetched() >= 256 * 1024,
            Duration::from_secs(30),
        )
        .await?;

        let slow = LimitsConfig {
            upload_bps: None,
            download_bps: bps(32 * KIB),
        };
        client
            .session
            .set_torrent_ratelimits(&client.handle, slow)
            .await?;
        assert_eq!(client.handle.ratelimits(), slow);
        let bytes = 192 * 1024;
        let took =
            time_to_move("downloads at the lowered rate", || client.fetched(), bytes).await?;
        assert!(
            took >= at_least(bytes, 32 * KIB),
            "{bytes} bytes in {took:?} at 32 KiB/s"
        );

        client
            .session
            .set_torrent_ratelimits(
                &client.handle,
                LimitsConfig {
                    upload_bps: None,
                    download_bps: bps(KIB),
                },
            )
            .await?;
        tokio::time::sleep(Duration::from_secs(1)).await;
        client
            .session
            .set_torrent_ratelimits(&client.handle, LimitsConfig::default())
            .await?;
        timeout(LIFTED_WITHIN, client.handle.wait_until_completed())
            .await
            .context("the transfer kept the old limit")??;
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_torrent_limit_set_while_paused_applies_after_resume() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder().await?;
        let client = start_client(
            &seed,
            AddTorrentOptions {
                paused: true,
                ..Default::default()
            },
        )
        .await?;
        assert!(client.handle.is_paused());
        assert_eq!(client.handle.ratelimits(), LimitsConfig::default());

        let slow = LimitsConfig {
            upload_bps: None,
            download_bps: bps(32 * KIB),
        };
        client
            .session
            .set_torrent_ratelimits(&client.handle, slow)
            .await?;
        assert_eq!(client.handle.ratelimits(), slow);

        let bytes = 160 * 1024;
        client.session.unpause(&client.handle).await?;
        let took = time_to_move("downloads after resume", || client.fetched(), bytes).await?;
        assert!(
            took >= at_least(bytes, 32 * KIB),
            "{bytes} bytes in {took:?} at 32 KiB/s"
        );

        // And it stays across another pause / resume.
        client.session.pause(&client.handle).await?;
        assert_eq!(client.handle.ratelimits(), slow);
        client.session.unpause(&client.handle).await?;
        assert_eq!(client.handle.ratelimits(), slow);
        let took = time_to_move(
            "downloads after a second resume",
            || client.fetched(),
            bytes,
        )
        .await?;
        assert!(
            took >= at_least(bytes, 32 * KIB),
            "{bytes} bytes in {took:?} at 32 KiB/s"
        );
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_torrent_limits_survive_a_session_restart() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder().await?;
        let root = TempDir::with_prefix("rqbit_ratelimit_persist")?;
        let persistence = || SessionOptions {
            dht: None,
            persistence: Some(SessionPersistenceConfig::Json {
                folder: Some(root.path().join("session")),
            }),
            listen: None,
            disable_local_service_discovery: true,
            ..Default::default()
        };

        let limits = LimitsConfig {
            upload_bps: bps(100 * KIB),
            download_bps: bps(200 * KIB),
        };
        {
            let session = Session::new_with_opts(root.path().join("out"), persistence()).await?;
            let handle = session
                .add_torrent(
                    AddTorrent::from_bytes(seed.torrent_bytes.clone()),
                    Some(AddTorrentOptions {
                        paused: true,
                        ..Default::default()
                    }),
                )
                .await?
                .into_handle()
                .context("into_handle")?;
            handle.wait_until_initialized().await?;
            session.set_torrent_ratelimits(&handle, limits).await?;
            session.stop().await;
        }

        let session = Session::new_with_opts(root.path().join("out"), persistence()).await?;
        let handle = session
            .get(TorrentIdOrHash::Hash(seed.info_hash))
            .context("the torrent was not restored")?;
        assert_eq!(handle.ratelimits(), limits);
        session.stop().await;
        Ok(())
    })
    .await
}
