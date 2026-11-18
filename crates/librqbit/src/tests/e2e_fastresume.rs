// FastResumeCheck::FileSizes trusts saved fastresume data without reading the
// files when every file is at its expected length, and falls back to hashing
// when one isn't.

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::Context;
use librqbit_core::lengths::ValidPieceIndex;

use crate::{
    AddTorrentOptions, FastResumeCheck, ManagedTorrentShared, Session, SessionOptions,
    SessionPersistenceConfig, create_torrent,
    spawn_utils::BlockingSpawner,
    storage::{
        BoxStorageFactory, StorageFactory, StorageFactoryExt, TorrentStorage,
        filesystem::FilesystemStorageFactory,
    },
    tests::test_util::{create_default_random_dir_with_torrents, setup_test_logging},
    torrent_state::TorrentMetadata,
};

/// Filesystem storage that counts reads.
#[derive(Clone, Default)]
struct CountingFactory {
    reads: Arc<AtomicU64>,
}

struct Counting {
    reads: Arc<AtomicU64>,
    underlying: Box<dyn TorrentStorage>,
}

impl StorageFactory for CountingFactory {
    type Storage = Counting;

    fn create(
        &self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<Self::Storage> {
        Ok(Counting {
            reads: self.reads.clone(),
            underlying: Box::new(FilesystemStorageFactory::default().create(shared, metadata)?),
        })
    }

    fn is_type_id(&self, type_id: std::any::TypeId) -> bool {
        FilesystemStorageFactory::default().is_type_id(type_id)
    }

    fn clone_box(&self) -> BoxStorageFactory {
        self.clone().boxed()
    }
}

impl TorrentStorage for Counting {
    fn init(
        &mut self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        self.underlying.init(shared, metadata)
    }

    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.underlying.pread_exact(file_id, offset, buf)
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        self.underlying.pwrite_all(file_id, offset, buf)
    }

    fn remove_file(&self, file_id: usize, filename: &Path) -> anyhow::Result<()> {
        self.underlying.remove_file(file_id, filename)
    }

    fn remove_directory_if_empty(&self, path: &Path) -> anyhow::Result<()> {
        self.underlying.remove_directory_if_empty(path)
    }

    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()> {
        self.underlying.ensure_file_length(file_id, length)
    }

    fn file_len(&self, file_id: usize) -> anyhow::Result<Option<u64>> {
        self.underlying.file_len(file_id)
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        Ok(Box::new(Counting {
            reads: self.reads.clone(),
            underlying: self.underlying.take()?,
        }))
    }

    fn on_piece_completed(&self, piece_index: ValidPieceIndex) -> anyhow::Result<()> {
        self.underlying.on_piece_completed(piece_index)
    }
}

async fn session(
    persistence: &Path,
    check: FastResumeCheck,
    storage: CountingFactory,
) -> anyhow::Result<Arc<Session>> {
    Session::new_with_opts(
        std::env::temp_dir().join("does_not_exist"),
        SessionOptions {
            dht: None,
            disable_local_service_discovery: true,
            listen: None,
            fastresume: true,
            persistence: Some(SessionPersistenceConfig::Json {
                folder: Some(persistence.to_owned()),
            }),
            fastresume_check: check,
            default_storage_factory: Some(storage.boxed()),
            ..Default::default()
        },
    )
    .await
}

/// Wait for the only torrent to finish initializing; returns (finished, have bytes).
async fn settled(session: &Session) -> anyhow::Result<(bool, u64)> {
    let h = session
        .with_torrents(|it| it.next().map(|(_, h)| h.clone()))
        .context("no torrent restored")?;
    for _ in 0..600 {
        let st = h.stats();
        if !matches!(st.state, crate::TorrentStatsState::Initializing { .. }) {
            return Ok((st.finished, st.progress_bytes));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("torrent didn't finish initializing")
}

#[tokio::test(flavor = "multi_thread")]
async fn test_fastresume_file_sizes() -> anyhow::Result<()> {
    setup_test_logging();
    let files = 3;
    let file_len = 1_000_000;
    let data = create_default_random_dir_with_torrents(files, file_len, Some("rqbit_fastresume"));
    let persistence = tempfile::TempDir::with_prefix("rqbit_fastresume_state")?;
    let torrent = create_torrent(
        data.path(),
        crate::CreateTorrentOptions {
            piece_length: Some(65536),
            ..Default::default()
        },
        &BlockingSpawner::new(1),
    )
    .await?;

    // 1. Add it: no fastresume data yet, so it is hashed in full and saved.
    {
        let s = session(
            persistence.path(),
            FastResumeCheck::Sample,
            Default::default(),
        )
        .await?;
        s.add_torrent(
            crate::AddTorrent::TorrentFileBytes(torrent.as_bytes()?),
            Some(AddTorrentOptions {
                overwrite: true,
                output_folder: Some(data.path().to_str().context("path")?.to_owned()),
                ..Default::default()
            }),
        )
        .await?;
        let (finished, _) = settled(&s).await?;
        assert!(finished, "the initial check found every piece");
        s.stop().await;
    }

    // 2. Restart with FileSizes: nothing is read, everything is still there.
    {
        let storage = CountingFactory::default();
        let s = session(
            persistence.path(),
            FastResumeCheck::FileSizes,
            storage.clone(),
        )
        .await?;
        let (finished, have) = settled(&s).await?;
        assert!(finished);
        assert_eq!(have, (files * file_len) as u64);
        assert_eq!(storage.reads.load(Ordering::Relaxed), 0, "no data read");
        s.stop().await;
    }

    // 3. Restart with Sample (the default): it reads to check.
    {
        let storage = CountingFactory::default();
        let s = session(persistence.path(), FastResumeCheck::Sample, storage.clone()).await?;
        let (finished, _) = settled(&s).await?;
        assert!(finished);
        assert!(storage.reads.load(Ordering::Relaxed) > 0, "sampling reads");
        s.stop().await;
    }

    // 4. A file shrank: FileSizes doesn't trust the bitfield, checks, and
    //    finds the missing piece.
    {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(data.path().join("1.data"))?;
        f.set_len(file_len as u64 - 1)?;
        drop(f);
        let storage = CountingFactory::default();
        let s = session(
            persistence.path(),
            FastResumeCheck::FileSizes,
            storage.clone(),
        )
        .await?;
        let (finished, _) = settled(&s).await?;
        assert!(
            storage.reads.load(Ordering::Relaxed) > 0,
            "fell back to reading"
        );
        assert!(!finished, "the truncated file's last piece is gone");
        s.stop().await;
    }
    Ok(())
}
