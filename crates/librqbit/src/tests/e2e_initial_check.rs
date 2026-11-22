use std::{
    path::Path,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
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

struct BlockingGate {
    entered: tokio::sync::Notify,
    entered_flag: AtomicBool,
    released: Mutex<bool>,
    cvar: Condvar,
}

impl BlockingGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: tokio::sync::Notify::new(),
            entered_flag: AtomicBool::new(false),
            released: Mutex::new(false),
            cvar: Condvar::new(),
        })
    }

    fn enter_and_wait(&self, timeout: Duration) {
        self.entered_flag.store(true, Ordering::SeqCst);
        self.entered.notify_waiters();
        let guard = self.released.lock().unwrap();
        let (_guard, wait_res) = self
            .cvar
            .wait_timeout_while(guard, timeout, |released| !*released)
            .unwrap();
        assert!(!wait_res.timed_out(), "timed out waiting on BlockingGate");
    }

    async fn wait_entered(&self) {
        loop {
            let notified = self.entered.notified();
            if self.entered_flag.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.cvar.notify_all();
    }
}

struct GateReleaseOnDrop(Vec<Arc<BlockingGate>>);

impl Drop for GateReleaseOnDrop {
    fn drop(&mut self) {
        for gate in &self.0 {
            gate.release();
        }
    }
}

#[derive(Clone)]
struct GatedStorageFactory {
    init_gate: Option<Arc<BlockingGate>>,
    read_gates: Arc<Vec<(usize, Arc<BlockingGate>)>>,
    reads_count: Arc<AtomicUsize>,
}

struct GatedStorage {
    inner: FilesystemStorage,
    init_gate: Option<Arc<BlockingGate>>,
    read_gates: Arc<Vec<(usize, Arc<BlockingGate>)>>,
    reads_count: Arc<AtomicUsize>,
}

impl StorageFactory for GatedStorageFactory {
    type Storage = GatedStorage;

    fn create(
        &self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<Self::Storage> {
        Ok(GatedStorage {
            inner: FilesystemStorageFactory::default().create(shared, metadata)?,
            init_gate: self.init_gate.clone(),
            read_gates: self.read_gates.clone(),
            reads_count: self.reads_count.clone(),
        })
    }

    fn clone_box(&self) -> crate::storage::BoxStorageFactory {
        self.clone().boxed()
    }
}

impl TorrentStorage for GatedStorage {
    fn init(
        &mut self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        if let Some(gate) = &self.init_gate {
            gate.enter_and_wait(Duration::from_secs(10));
        }
        self.inner.init(shared, metadata)
    }

    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        let read_num = self.reads_count.fetch_add(1, Ordering::SeqCst) + 1;
        for (target_read, gate) in self.read_gates.iter() {
            if *target_read == read_num {
                gate.enter_and_wait(Duration::from_secs(10));
            }
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
            init_gate: self.init_gate.clone(),
            read_gates: self.read_gates.clone(),
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
        // 24 x 512 KiB = 12 MiB = 192 x 64 KiB reads (>= 3 slices of 4 MiB / 64 reads).
        let piece_len = 512 * 1024u32;
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

        let slice1_gate = BlockingGate::new();
        let slice2_gate = BlockingGate::new();
        let _guard = GateReleaseOnDrop(vec![slice1_gate.clone(), slice2_gate.clone()]);

        // On `new_current_thread`, an inline synchronous check would freeze the event loop
        // inside `slice1_gate.enter_and_wait` and never poll `driver` below.
        let driver = {
            let session = session.clone();
            let slice1_gate = slice1_gate.clone();
            let slice2_gate = slice2_gate.clone();
            tokio::spawn(async move {
                tokio::time::timeout(Duration::from_secs(10), slice1_gate.wait_entered())
                    .await
                    .expect("single-thread event loop stalled before slice 1 gate");
                tokio::time::sleep(Duration::from_millis(1)).await;
                let count = session.with_torrents(|it| it.count());
                assert_eq!(count, 1);
                slice1_gate.release();

                tokio::time::timeout(Duration::from_secs(10), slice2_gate.wait_entered())
                    .await
                    .expect("single-thread event loop stalled before slice 2 gate");
                tokio::time::sleep(Duration::from_millis(1)).await;
                slice2_gate.release();
            })
        };

        let storage_factory = GatedStorageFactory {
            init_gate: None,
            read_gates: Arc::new(vec![(1, slice1_gate), (65, slice2_gate)]),
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

        driver.await.unwrap();

        wait_until(
            || {
                let stats = handle.stats();
                if !stats.finished {
                    anyhow::bail!("waiting for initial check: {:?}", stats.state);
                }
                Ok(())
            },
            Duration::from_secs(30),
        )
        .await
        .unwrap();
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
        // 24 x 512 KiB = 12 MiB = 192 x 64 KiB reads (at most 64 reads per 4 MiB slice).
        let piece_len = 512 * 1024u32;
        create_new_file_with_random_content(
            &dir.path().join("existing.bin"),
            piece_len as usize * 24,
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
        // storage init nor monopolizes the sole BlockingSpawner permit across all 192 reads.
        let session = Session::new_with_opts(dir.path().to_owned(), minimal_session_opts(1))
            .await
            .unwrap();

        let init_gate = BlockingGate::new();
        let read1_gate = BlockingGate::new();
        let _guard = GateReleaseOnDrop(vec![init_gate.clone(), read1_gate.clone()]);

        let reads_count = Arc::new(AtomicUsize::new(0));
        let storage_factory = GatedStorageFactory {
            init_gate: Some(init_gate.clone()),
            read_gates: Arc::new(vec![(1, read1_gate.clone())]),
            reads_count: reads_count.clone(),
        };

        let add_task = {
            let session = session.clone();
            tokio::spawn(async move {
                session
                    .add_torrent(
                        AddTorrent::from_bytes(torrent_bytes),
                        Some(AddTorrentOptions {
                            overwrite: true,
                            storage_factory: Some(storage_factory.boxed()),
                            ..Default::default()
                        }),
                    )
                    .await
            })
        };

        // Wait until `add_torrent` is paused inside `Storage::init()`, then prove `db.read()`
        // (`with_torrents`) is not blocked by `db.write()`.
        tokio::time::timeout(Duration::from_secs(10), init_gate.wait_entered())
            .await
            .expect("timed out waiting for storage.init");

        let session_for_read = session.clone();
        let read_probe = tokio::task::spawn_blocking(move || {
            session_for_read.with_torrents(|it| it.count())
        });
        let count_during_init = tokio::time::timeout(Duration::from_secs(2), read_probe)
            .await
            .expect("db.read() blocked while storage.init was running")
            .unwrap();
        assert_eq!(count_during_init, 0);

        init_gate.release();

        let handle = add_task.await.unwrap().unwrap().into_handle().unwrap();

        // Wait until slice 1 holds the sole BlockingSpawner permit at read #1, enqueue a
        // waiter on the spawner semaphore, then release read #1 and verify our waiter runs
        // at or before the end of slice 1 (<= 64 reads) rather than after all 192 reads.
        tokio::time::timeout(Duration::from_secs(10), read1_gate.wait_entered())
            .await
            .expect("timed out waiting for first check read");

        let probe_enqueued = Arc::new(tokio::sync::Notify::new());
        let probe_task = {
            let spawner = handle.shared.spawner.clone();
            let reads_count = reads_count.clone();
            let handle = handle.clone();
            let probe_enqueued = probe_enqueued.clone();
            tokio::spawn(async move {
                let fut = spawner.spawn_blocking(move || {
                    (reads_count.load(Ordering::SeqCst), handle.stats().finished)
                });
                tokio::pin!(fut);
                // Poll once so this task registers in the semaphore's FIFO waiter queue
                // while `read1_gate` still holds the sole permit.
                std::future::poll_fn(|cx| {
                    assert!(
                        fut.as_mut().poll(cx).is_pending(),
                        "spawner permit was unexpectedly free during active slice"
                    );
                    std::task::Poll::Ready(())
                })
                .await;
                probe_enqueued.notify_one();
                fut.await.unwrap()
            })
        };

        probe_enqueued.notified().await;
        read1_gate.release();

        let (reads_at_probe, finished_at_probe) =
            tokio::time::timeout(Duration::from_secs(30), probe_task)
                .await
                .expect("timed out waiting for spawner permit turnover")
                .unwrap();

        assert!(
            !finished_at_probe,
            "initial check monopolized spawner permit until completion"
        );
        assert!(
            reads_at_probe <= 64,
            "expected spawner permit to turn over within one 4 MiB slice (<= 64 reads), got {reads_at_probe}/192 reads"
        );

        wait_until(
            || {
                if handle.stats().finished {
                    Ok(())
                } else {
                    anyhow::bail!("waiting for check completion")
                }
            },
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_initial_check_pause_aborts_mid_large_piece_and_resumes_cleanly() {
    let dir = TempDir::new().unwrap();
    // Single 1 MiB piece = 16 x 64 KiB reads.
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

    let read2_gate = BlockingGate::new();
    let _guard = GateReleaseOnDrop(vec![read2_gate.clone()]);
    let reads_count = Arc::new(AtomicUsize::new(0));
    let storage_factory = GatedStorageFactory {
        init_gate: None,
        read_gates: Arc::new(vec![(2, read2_gate.clone())]),
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

    // Deterministically pause while chunk 2 of 16 inside piece 0 is in flight.
    tokio::time::timeout(Duration::from_secs(10), read2_gate.wait_entered())
        .await
        .expect("timed out waiting for chunk 2 read");

    session.pause(&handle).await.unwrap();
    read2_gate.release();

    // Wait until the aborted blocking check slice exits and releases its spawner permit.
    wait_until(
        || {
            if handle.shared.spawner.semaphore().available_permits() < 2 {
                anyhow::bail!("waiting for aborted check slice to exit");
            }
            Ok(())
        },
        Duration::from_secs(10),
    )
    .await
    .unwrap();

    let reads_after_pause = reads_count.load(Ordering::SeqCst);
    assert_eq!(
        reads_after_pause, 2,
        "expected pause to abort immediately after chunk 2/16, got {reads_after_pause}/16 reads"
    );

    // Unpause; progress must reset cleanly and finish at exact total_bytes.
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
        Duration::from_secs(30),
    )
    .await
    .unwrap();
}
