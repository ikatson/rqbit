use std::{
    collections::HashSet,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, ensure};
use parking_lot::RwLock;
use tokio::time::timeout;

use crate::{
    AddTorrent, AddTorrentOptions, ManagedTorrent, ManagedTorrentShared, Session, SessionOptions,
    TorrentMetadata,
    storage::{BoxStorageFactory, StorageFactory, StorageFactoryExt, TorrentStorage},
    tests::test_util::{setup_test_logging, wait_until},
};

const PIECE_LENGTH: usize = 16384;
const TEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct MemoryStorageFactory {
    data: Arc<RwLock<Vec<u8>>>,
    reads: Arc<AtomicUsize>,
    external: bool,
}

impl MemoryStorageFactory {
    fn new(data: Vec<u8>, external: bool) -> Self {
        Self {
            data: Arc::new(RwLock::new(data)),
            reads: Arc::new(AtomicUsize::new(0)),
            external,
        }
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }
}

struct MemoryStorage {
    factory: MemoryStorageFactory,
    active: RwLock<bool>,
}

impl StorageFactory for MemoryStorageFactory {
    type Storage = MemoryStorage;

    fn create(
        &self,
        _: &ManagedTorrentShared,
        _: &TorrentMetadata,
    ) -> anyhow::Result<Self::Storage> {
        Ok(MemoryStorage {
            factory: self.clone(),
            active: RwLock::new(true),
        })
    }

    fn clone_box(&self) -> BoxStorageFactory {
        self.clone().boxed()
    }
}

impl TorrentStorage for MemoryStorage {
    fn externally_populated(&self) -> bool {
        self.factory.external
    }

    fn init(&mut self, _: &ManagedTorrentShared, _: &TorrentMetadata) -> anyhow::Result<()> {
        Ok(())
    }

    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        let active = self.active.read();
        ensure!(*active, "storage was taken");
        ensure!(file_id == 0, "invalid file");
        self.factory.reads.fetch_add(1, Ordering::Relaxed);
        let data = self.factory.data.read();
        let offset = usize::try_from(offset)?;
        buf.copy_from_slice(
            data.get(offset..offset + buf.len())
                .context("piece is not available yet")?,
        );
        Ok(())
    }

    fn pwrite_all(&self, _: usize, _: u64, _: &[u8]) -> anyhow::Result<()> {
        panic!("seed storage must never receive downloaded data");
    }

    fn remove_file(&self, _: usize, _: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    fn remove_directory_if_empty(&self, _: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    fn ensure_file_length(&self, _: usize, _: u64) -> anyhow::Result<()> {
        ensure!(!self.factory.external, "seed storage must not be resized");
        Ok(())
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        let mut active = self.active.write();
        ensure!(*active, "storage was already taken");
        *active = false;
        Ok(Box::new(Self {
            factory: self.factory.clone(),
            active: RwLock::new(true),
        }))
    }
}

fn session_options() -> SessionOptions {
    SessionOptions {
        dht: None,
        disable_local_service_discovery: true,
        ..Default::default()
    }
}

fn have_pieces(handle: &ManagedTorrent) -> Vec<usize> {
    handle
        .with_chunk_tracker(|ct| ct.get_have_pieces().as_slice().iter_ones().collect())
        .unwrap()
}

async fn fixture() -> anyhow::Result<(tempfile::TempDir, bytes::Bytes, Vec<u8>)> {
    let root = tempfile::TempDir::with_prefix("rqbit_external_storage")?;
    let path = root.path().join("data");
    // Two full pieces and a short final piece, with different contents per piece.
    let data: Vec<_> = (0..2 * PIECE_LENGTH + 123)
        .map(|offset| u8::try_from(offset / PIECE_LENGTH + 1).unwrap())
        .collect();
    std::fs::write(&path, &data)?;
    let torrent = crate::create_torrent(
        &path,
        crate::CreateTorrentOptions {
            piece_length: Some(PIECE_LENGTH.try_into()?),
            ..Default::default()
        },
        &crate::spawn_utils::BlockingSpawner::new(1),
    )
    .await?;
    Ok((root, torrent.as_bytes()?, data))
}

#[tokio::test(flavor = "multi_thread")]
async fn test_external_storage_verification() -> anyhow::Result<()> {
    setup_test_logging();
    let (root, torrent, data) = fixture().await?;
    // The final piece is initially missing, and the second piece is corrupt.
    let mut available = data[..2 * PIECE_LENGTH].to_vec();
    available[PIECE_LENGTH] ^= 1;
    let storage = MemoryStorageFactory::new(available, true);
    let session = Session::new_with_opts(root.path().join("seed"), session_options()).await?;
    let handle = session
        .add_torrent(
            AddTorrent::from_bytes(torrent),
            Some(AddTorrentOptions {
                storage_factory: Some(storage.clone().boxed()),
                only_files: Some(vec![]),
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .context("expected torrent handle")?;
    timeout(TEST_TIMEOUT, handle.wait_until_initialized()).await??;
    assert_eq!(storage.reads(), 0);
    assert!(have_pieces(&handle).is_empty());
    let live = handle.live().context("expected live torrent")?;
    assert!(live.recheck_external_piece(3).is_err());
    assert_eq!(storage.reads(), 0);

    assert!(live.recheck_external_piece(0)?);
    assert_eq!(have_pieces(&handle), vec![0]);
    let reads = storage.reads();
    assert!(reads > 0);
    assert!(!live.recheck_external_piece(0)?);
    assert_eq!(storage.reads(), reads);
    assert!(
        live.recheck_external_piece(1)
            .unwrap_err()
            .to_string()
            .contains("hash mismatch")
    );
    assert!(live.recheck_external_piece(2).is_err());
    assert_eq!(have_pieces(&handle), vec![0]);
    assert_eq!(live.get_hns().unwrap().have_bytes, PIECE_LENGTH as u64);
    assert_eq!(live.get_approx_have_bytes(), PIECE_LENGTH as u64);
    assert_eq!(handle.stats().file_progress, vec![PIECE_LENGTH as u64]);

    assert!(
        session
            .update_only_files(&handle, &HashSet::from([0]))
            .await
            .is_err()
    );
    session.update_only_files(&handle, &HashSet::new()).await?;
    *storage.data.write() = data.clone();
    assert!(live.recheck_external_piece(1)?);
    assert!(live.recheck_external_piece(2)?);
    assert_eq!(have_pieces(&handle), vec![0, 1, 2]);
    assert_eq!(handle.stats().file_progress, vec![data.len() as u64]);
    assert_eq!(live.get_approx_have_bytes(), data.len() as u64);
    assert_eq!(live.get_downloaded_bytes(), 0);

    session.pause(&handle).await?;
    assert!(handle.live().is_none());
    let reads = storage.reads();
    assert!(live.recheck_external_piece(0).is_err());
    assert_eq!(storage.reads(), reads);
    assert!(
        session
            .update_only_files(&handle, &HashSet::from([0]))
            .await
            .is_err()
    );
    session.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_external_storage_rejects_selected_downloads() -> anyhow::Result<()> {
    let (root, torrent, data) = fixture().await?;
    for selection in [None, Some(vec![0])] {
        let storage = MemoryStorageFactory::new(data.clone(), true);
        let session = Session::new_with_opts(root.path().join("seed"), session_options()).await?;
        let handle = session
            .add_torrent(
                AddTorrent::from_bytes(torrent.clone()),
                Some(AddTorrentOptions {
                    storage_factory: Some(storage.clone().boxed()),
                    only_files: selection,
                    ..Default::default()
                }),
            )
            .await?
            .into_handle()
            .context("expected torrent handle")?;
        let error = timeout(TEST_TIMEOUT, handle.wait_until_initialized())
            .await?
            .unwrap_err();
        assert!(error.to_string().contains("no selected downloads"));
        assert_eq!(storage.reads(), 0);
        assert!(handle.live().is_none());
        session.stop().await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_ordinary_storage_still_checks_initial_data() -> anyhow::Result<()> {
    let (root, torrent, data) = fixture().await?;
    let storage = MemoryStorageFactory::new(data, false);
    let session = Session::new_with_opts(root.path().join("seed"), session_options()).await?;
    let handle = session
        .add_torrent(
            AddTorrent::from_bytes(torrent),
            Some(AddTorrentOptions {
                storage_factory: Some(storage.clone().boxed()),
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .context("expected torrent handle")?;
    timeout(TEST_TIMEOUT, handle.wait_until_initialized()).await??;
    assert!(storage.reads() > 0);
    assert_eq!(have_pieces(&handle), vec![0, 1, 2]);
    let reads = storage.reads();
    assert!(handle.live().unwrap().recheck_external_piece(0).is_err());
    assert_eq!(storage.reads(), reads);
    session.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_external_storage_e2e_have_on_existing_connection() -> anyhow::Result<()> {
    timeout(TEST_TIMEOUT, async {
        setup_test_logging();
        let (root, torrent, data) = fixture().await?;
        let storage = MemoryStorageFactory::new(Vec::new(), true);
        let seeder = Session::new_with_opts(
            root.path().join("seed"),
            SessionOptions {
                listen: Some(crate::listen::ListenerOptions {
                    mode: crate::ListenerMode::TcpOnly,
                    listen_addr: ([127, 0, 0, 1], 0).into(),
                    ..Default::default()
                }),
                ..session_options()
            },
        )
        .await?;
        let seed = seeder
            .add_torrent(
                AddTorrent::from_bytes(torrent.clone()),
                Some(AddTorrentOptions {
                    storage_factory: Some(storage.clone().boxed()),
                    only_files: Some(vec![]),
                    ..Default::default()
                }),
            )
            .await?
            .into_handle()
            .context("expected seeder handle")?;
        seed.wait_until_initialized().await?;
        let live = seed.live().context("expected live seeder")?;
        let leecher =
            Session::new_with_opts(root.path().join("download"), session_options()).await?;
        let download = leecher
            .add_torrent(
                AddTorrent::from_bytes(torrent),
                Some(AddTorrentOptions {
                    initial_peers: Some(vec![
                        seeder.listen_addr().context("expected listen address")?,
                    ]),
                    ..Default::default()
                }),
            )
            .await?
            .into_handle()
            .context("expected leecher handle")?;
        download.wait_until_initialized().await?;
        wait_until(
            || {
                ensure!(
                    live.stats_snapshot().peer_stats.live == 1,
                    "leecher is not connected"
                );
                Ok(())
            },
            TEST_TIMEOUT,
        )
        .await?;
        assert_eq!(storage.reads(), 0);
        assert!(have_pieces(&download).is_empty());
        // A stream must not cause the seed-only session to request missing pieces.
        let _stream = seed.clone().stream(0).await?;
        for piece in 0..3 {
            let end = ((piece + 1) * PIECE_LENGTH).min(data.len());
            *storage.data.write() = data[..end].to_vec();
            assert!(live.recheck_external_piece(piece.try_into()?)?);
            wait_until(
                || {
                    ensure!(
                        have_pieces(&download).contains(&piece),
                        "waiting for piece {piece}"
                    );
                    Ok(())
                },
                TEST_TIMEOUT,
            )
            .await?;
            assert!(Arc::ptr_eq(&live, &seed.live().context("seeder stopped")?));
        }
        download.wait_until_completed().await?;
        assert_eq!(std::fs::read(root.path().join("download/data"))?, data);
        // Keep all peer states: normal completion may disconnect the finished leecher.
        let peers = live.per_peer_stats_snapshot(
            crate::torrent_state::peer::stats::snapshot::PeerStatsFilter {
                state: crate::torrent_state::peer::stats::snapshot::PeerStatsFilterState::All,
            },
        );
        // PEX can add other entries, including the seed's own address. Count actual
        // incoming connections rather than peer-table entries.
        assert_eq!(
            peers
                .peers
                .values()
                .map(|p| p.counters.incoming_connections)
                .sum::<u32>(),
            1,
        );
        let download_peers = download
            .live()
            .context("leecher stopped")?
            .per_peer_stats_snapshot(
                crate::torrent_state::peer::stats::snapshot::PeerStatsFilter {
                    state: crate::torrent_state::peer::stats::snapshot::PeerStatsFilterState::All,
                },
            );
        let seed_address = seeder.listen_addr().unwrap().to_string();
        let peer = &download_peers.peers[&seed_address];
        assert_eq!(peer.counters.connections, 1);
        assert_eq!(peer.counters.connection_attempts, 1);
        assert_eq!(live.stats_snapshot().fetched_bytes, 0);
        seeder.stop().await;
        leecher.stop().await;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("external-storage transfer timed out")?
}

#[tokio::test(flavor = "multi_thread")]
async fn test_external_storage_ignores_fastresume() -> anyhow::Result<()> {
    let (root, torrent, data) = fixture().await?;
    let storage = MemoryStorageFactory::new(data, true);
    let session = Session::new_with_opts(
        root.path().join("seed"),
        SessionOptions {
            persistence: Some(crate::SessionPersistenceConfig::Json {
                folder: Some(root.path().join("session")),
            }),
            fastresume: true,
            ..session_options()
        },
    )
    .await?;
    // Session persistence only accepts filesystem factories. Use its metadata and
    // bitfield store to initialize external storage directly below.
    let handle = session
        .add_torrent(
            AddTorrent::from_bytes(torrent),
            Some(AddTorrentOptions {
                only_files: Some(vec![]),
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .context("expected torrent handle")?;
    timeout(TEST_TIMEOUT, handle.wait_until_initialized()).await??;
    let metadata = handle.metadata.load_full().context("expected metadata")?;
    // Deliberately persist stale HAVE bits, then initialize another storage instance.
    // Retain this bitfield until after the assertions so its drop cannot flush over
    // the newly initialized bitfield while the check is running.
    let stale = session
        .bitv_factory
        .store_initial_check(
            handle.info_hash().into(),
            crate::type_aliases::BF::from_boxed_slice(vec![0xe0].into_boxed_slice()),
        )
        .await?;
    for selection in [None, Some(vec![0]), Some(vec![])] {
        let initializing = crate::torrent_state::initializing::TorrentStateInitializing::new(
            handle.shared.clone(),
            metadata.clone(),
            selection.clone(),
            Box::new(storage.create_and_init(&handle.shared, &metadata)?),
            false,
        );
        let result = timeout(TEST_TIMEOUT, initializing.check()).await?;
        if selection.as_ref().is_some_and(|files| files.is_empty()) {
            let paused = result?;
            assert!(!paused.chunk_tracker.get_have_pieces().as_slice().any());
        } else {
            assert!(result.is_err());
        }
        assert_eq!(storage.reads(), 0);
    }
    drop(stale);
    session.stop().await;
    Ok(())
}
