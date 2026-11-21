use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use tempfile::TempDir;

use crate::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, Session, SessionOptions,
    TorrentStatsState, create_torrent,
    spawn_utils::BlockingSpawner,
    storage::{
        StorageFactory, StorageFactoryExt, TorrentStorage,
        filesystem::{FilesystemStorage, FilesystemStorageFactory},
    },
    tests::test_util::{create_new_file_with_random_content, wait_until},
    torrent_state::{ManagedTorrentShared, TorrentMetadata},
};

#[derive(Clone)]
struct SlowStorageFactory {
    read_delay_ms: Arc<AtomicU64>,
    init_delay_ms: u64,
    reads_count: Arc<AtomicUsize>,
}

struct SlowStorage {
    inner: FilesystemStorage,
    read_delay_ms: Arc<AtomicU64>,
    init_delay_ms: u64,
    reads_count: Arc<AtomicUsize>,
}

impl StorageFactory for SlowStorageFactory {
    type Storage = SlowStorage;

    fn create(
        &self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<Self::Storage> {
        Ok(SlowStorage {
            inner: FilesystemStorageFactory::default().create(shared, metadata)?,
            read_delay_ms: self.read_delay_ms.clone(),
            init_delay_ms: self.init_delay_ms,
            reads_count: self.reads_count.clone(),
        })
    }

    fn clone_box(&self) -> crate::storage::BoxStorageFactory {
        self.clone().boxed()
    }
}

impl TorrentStorage for SlowStorage {
    fn init(
        &mut self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        if self.init_delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(self.init_delay_ms));
        }
        self.inner.init(shared, metadata)
    }

    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        self.reads_count.fetch_add(1, Ordering::Relaxed);
        let delay = self.read_delay_ms.load(Ordering::Relaxed);
        if delay > 0 {
            std::thread::sleep(Duration::from_millis(delay));
        }
        self.inner.pread_exact(file_id, offset, buf)
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        self.inner.pwrite_all(file_id, offset, buf)
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
        Ok(Box::new(Self {
            inner: self.inner.take_fs()?,
            read_delay_ms: self.read_delay_ms.clone(),
            init_delay_ms: self.init_delay_ms,
            reads_count: self.reads_count.clone(),
        }))
    }
}

fn minimal_session_opts(runtime_worker_threads: usize) -> SessionOptions {
    SessionOptions {
        dht: None,
        disable_trackers: true,
        disable_local_service_discovery: true,
        listen: None,
        persistence: None,
        runtime_worker_threads: Some(runtime_worker_threads),
        ..Default::default()
    }
}

#[test]
fn test_initial_check_single_thread_runtime_stays_responsive() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(4)
        .build()
        .unwrap();

    rt.block_on(async {
        let dir = TempDir::new().unwrap();
        let piece_len = 65536u32;
        let total_size = piece_len as usize * 24;
        create_new_file_with_random_content(&dir.path().join("existing.bin"), total_size);

        let torrent = create_torrent(
            dir.path(),
            CreateTorrentOptions {
                piece_length: Some(piece_len),
                ..Default::default()
            },
            &BlockingSpawner::new(1),
        )
        .await
        .unwrap();
        let torrent_bytes = torrent.as_bytes().unwrap();

        let session = Session::new_with_opts(dir.path().to_owned(), minimal_session_opts(1))
            .await
            .unwrap();

        let max_tick_gap_ms = Arc::new(AtomicU64::new(0));
        let tick_count = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let heartbeat = {
            let max_tick_gap_ms = max_tick_gap_ms.clone();
            let tick_count = tick_count.clone();
            let stop = stop.clone();
            tokio::spawn(async move {
                let mut last = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    let gap = u64::try_from(last.elapsed().as_millis()).unwrap_or(u64::MAX);
                    max_tick_gap_ms.fetch_max(gap, Ordering::Relaxed);
                    tick_count.fetch_add(1, Ordering::Relaxed);
                    last = Instant::now();
                }
            })
        };

        let storage_factory = SlowStorageFactory {
            read_delay_ms: Arc::new(AtomicU64::new(6)),
            init_delay_ms: 0,
            reads_count: Arc::new(AtomicUsize::new(0)),
        };

        let handle = session
            .add_torrent(
                AddTorrent::from_bytes(torrent_bytes),
                Some(AddTorrentOptions {
                    overwrite: true,
                    storage_factory: Some(storage_factory.boxed()),
                    ..Default::default()
                }),
            )
            .await
            .unwrap()
            .into_handle()
            .unwrap();

        wait_until(
            || {
                let stats = handle.stats();
                if !stats.finished {
                    anyhow::bail!("waiting for initial check: {:?}", stats.state);
                }
                Ok(())
            },
            Duration::from_secs(10),
        )
        .await
        .unwrap();

        stop.store(true, Ordering::Relaxed);
        heartbeat.await.unwrap();

        // Total check takes >= 144ms (24 * 6ms). Previously on current_thread, the single-thread
        // reactor stalled for the entire check (0 ticks during check, gap >= 144ms).
        assert!(
            tick_count.load(Ordering::Relaxed) >= 10,
            "expected event loop to keep ticking during check, got {} ticks",
            tick_count.load(Ordering::Relaxed)
        );
        assert!(
            max_tick_gap_ms.load(Ordering::Relaxed) < 80,
            "single-thread runtime stalled for {}ms during initial check",
            max_tick_gap_ms.load(Ordering::Relaxed)
        );
    });
}

#[test]
fn test_initial_check_yields_spawner_and_add_does_not_hold_db_write_lock() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(4)
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let dir = TempDir::new().unwrap();
        let piece_len = 65536u32;
        create_new_file_with_random_content(
            &dir.path().join("existing.bin"),
            piece_len as usize * 20,
        );

        let torrent = create_torrent(
            dir.path(),
            CreateTorrentOptions {
                piece_length: Some(piece_len),
                ..Default::default()
            },
            &BlockingSpawner::new(1),
        )
        .await
        .unwrap();
        let torrent_bytes = torrent.as_bytes().unwrap();

        // Only 1 BlockingSpawner permit: verify add_torrent neither holds `db.write()` during
        // slow storage init nor monopolizes the sole BlockingSpawner permit for the entire check.
        let session = Session::new_with_opts(dir.path().to_owned(), minimal_session_opts(1))
            .await
            .unwrap();

        let reads_count = Arc::new(AtomicUsize::new(0));
        let storage_factory = SlowStorageFactory {
            read_delay_ms: Arc::new(AtomicU64::new(8)),
            init_delay_ms: 90,
            reads_count: reads_count.clone(),
        };

        // Probe `with_torrents` (`db.read()`) while `add_torrent` is inside `storage.init` (90ms).
        let session_for_read = session.clone();
        let db_read_probe = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let start = Instant::now();
            session_for_read.with_torrents(|_| ());
            start.elapsed()
        });

        let handle = session
            .add_torrent(
                AddTorrent::from_bytes(torrent_bytes),
                Some(AddTorrentOptions {
                    overwrite: true,
                    storage_factory: Some(storage_factory.boxed()),
                    ..Default::default()
                }),
            )
            .await
            .unwrap()
            .into_handle()
            .unwrap();

        let db_read_latency = db_read_probe.await.unwrap();
        assert!(
            db_read_latency < Duration::from_millis(40),
            "db.read() was blocked for {db_read_latency:?} during storage initialization"
        );

        // Wait until check is actively reading pieces, then verify the single spawner permit
        // turns over within one slice (~20ms) instead of blocking until all 20 pieces (~160ms) finish.
        wait_until(
            || {
                if reads_count.load(Ordering::Relaxed) >= 2 {
                    Ok(())
                } else {
                    anyhow::bail!("waiting for reads to start")
                }
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap();

        let permit_wait_start = Instant::now();
        handle.shared.spawner.spawn_blocking(|| {}).await.unwrap();
        let permit_wait = permit_wait_start.elapsed();
        assert!(
            !handle.stats().finished,
            "check already finished before spawner turnover was measured"
        );
        assert!(
            permit_wait < Duration::from_millis(80),
            "spawner permit was monopolized for {permit_wait:?}"
        );

        wait_until(
            || {
                if handle.stats().finished {
                    Ok(())
                } else {
                    anyhow::bail!("waiting for check completion")
                }
            },
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_initial_check_pause_aborts_mid_large_piece_and_resumes_cleanly() {
    let dir = TempDir::new().unwrap();
    // Single 1 MiB piece = 16 x 64 KiB reads. At 25ms per read, a full piece would take 400ms.
    let piece_len = 1024 * 1024u32;
    create_new_file_with_random_content(&dir.path().join("large_piece.bin"), piece_len as usize);

    let torrent = create_torrent(
        dir.path(),
        CreateTorrentOptions {
            piece_length: Some(piece_len),
            ..Default::default()
        },
        &BlockingSpawner::new(1),
    )
    .await
    .unwrap();
    let torrent_bytes = torrent.as_bytes().unwrap();

    let session = Session::new_with_opts(dir.path().to_owned(), minimal_session_opts(2))
        .await
        .unwrap();

    let read_delay_ms = Arc::new(AtomicU64::new(25));
    let reads_count = Arc::new(AtomicUsize::new(0));
    let storage_factory = SlowStorageFactory {
        read_delay_ms: read_delay_ms.clone(),
        init_delay_ms: 0,
        reads_count: reads_count.clone(),
    };

    let handle = session
        .add_torrent(
            AddTorrent::from_bytes(torrent_bytes),
            Some(AddTorrentOptions {
                overwrite: true,
                storage_factory: Some(storage_factory.boxed()),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .into_handle()
        .unwrap();

    // Wait until 2 of the 16 chunks inside piece 0 have been read, then pause mid-piece.
    wait_until(
        || {
            if reads_count.load(Ordering::Relaxed) >= 2 {
                Ok(())
            } else {
                anyhow::bail!("waiting for mid-piece read")
            }
        },
        Duration::from_secs(5),
    )
    .await
    .unwrap();

    let pause_start = Instant::now();
    session.pause(&handle).await.unwrap();
    wait_until(
        || {
            let is_running = handle.with_state(|s| match s {
                crate::ManagedTorrentState::Initializing(init) => !init.is_pause_requested(),
                _ => true,
            });
            if is_running {
                anyhow::bail!("expected paused initializing state");
            }
            Ok(())
        },
        Duration::from_secs(2),
    )
    .await
    .unwrap();

    // Wait briefly to confirm reads actually stopped mid-piece instead of reading all 16 chunks.
    tokio::time::sleep(Duration::from_millis(60)).await;
    let reads_after_pause = reads_count.load(Ordering::Relaxed);
    assert!(
        reads_after_pause < 10,
        "expected pause to abort mid-piece, but {reads_after_pause}/16 chunks were read in {:?}",
        pause_start.elapsed()
    );

    // Speed up reads and unpause; progress must reset cleanly and finish at exact total_bytes.
    read_delay_ms.store(0, Ordering::Relaxed);
    session.unpause(&handle).await.unwrap();

    wait_until(
        || {
            let stats = handle.stats();
            if stats.finished && matches!(stats.state, TorrentStatsState::Live) {
                assert_eq!(stats.progress_bytes, stats.total_bytes);
                Ok(())
            } else {
                anyhow::bail!("waiting for resumed check to finish: {:?}", stats.state)
            }
        },
        Duration::from_secs(5),
    )
    .await
    .unwrap();
}
