// on_piece_completed() is the storage's word that the piece is there to stay. It used to
// be asked after the have-bit was set, and a refusal was logged at debug and ignored: the
// torrent finished, advertised every piece and served them, over bytes the storage had
// refused. A refused commit is a disk failure like a failed write, and ends the torrent the
// same way, before the piece is anyone's.

use std::{net::Ipv4Addr, time::Duration};

use anyhow::Context;
use librqbit_core::{constants::CHUNK_SIZE, lengths::ValidPieceIndex};
use tempfile::TempDir;
use tokio::time::timeout;

use crate::{
    AddTorrent, CreateTorrentOptions, ManagedTorrentShared, Session, create_torrent,
    spawn_utils::BlockingSpawner,
    storage::{
        BoxStorageFactory, StorageFactory, StorageFactoryExt, TorrentStorage,
        filesystem::FilesystemStorageFactory,
    },
    tests::test_util::{
        TestPeerMetadata, create_default_random_dir_with_torrents, setup_test_logging,
    },
    torrent_state::TorrentMetadata,
};

const PIECE_LEN: u32 = CHUNK_SIZE;
const FILE_SIZE: usize = (PIECE_LEN * 16) as usize;

// A storage that takes every chunk and refuses to commit any piece: what a full disk or a
// directory that won't take a rename looks like to a storage that stages pieces.
#[derive(Clone, Default)]
struct RefusingCommitStorageFactory {
    underlying: FilesystemStorageFactory,
    attempts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl StorageFactory for RefusingCommitStorageFactory {
    type Storage = RefusingCommitStorage;

    fn create(
        &self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<Self::Storage> {
        Ok(RefusingCommitStorage {
            underlying: Box::new(self.underlying.create(shared, metadata)?),
            attempts: self.attempts.clone(),
        })
    }

    fn clone_box(&self) -> BoxStorageFactory {
        self.clone().boxed()
    }
}

struct RefusingCommitStorage {
    underlying: Box<dyn TorrentStorage>,
    attempts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl TorrentStorage for RefusingCommitStorage {
    fn init(
        &mut self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        self.underlying.init(shared, metadata)
    }

    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        self.underlying.pread_exact(file_id, offset, buf)
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        self.underlying.pwrite_all(file_id, offset, buf)
    }

    fn remove_file(&self, file_id: usize, filename: &std::path::Path) -> anyhow::Result<()> {
        self.underlying.remove_file(file_id, filename)
    }

    fn remove_directory_if_empty(&self, path: &std::path::Path) -> anyhow::Result<()> {
        self.underlying.remove_directory_if_empty(path)
    }

    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()> {
        self.underlying.ensure_file_length(file_id, length)
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        Ok(Box::new(RefusingCommitStorage {
            underlying: self.underlying.take()?,
            attempts: self.attempts.clone(),
        }))
    }

    fn on_piece_completed(&self, piece_index: ValidPieceIndex) -> anyhow::Result<()> {
        self.attempts
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        anyhow::bail!("refusing to commit piece {piece_index}: no space left on device")
    }
}

async fn e2e_refused_commit_is_fatal() -> anyhow::Result<()> {
    setup_test_logging();
    let files = create_default_random_dir_with_torrents(1, FILE_SIZE, Some("test_refused_commit"));
    let torrent = create_torrent(
        files.path(),
        CreateTorrentOptions {
            name: None,
            piece_length: Some(PIECE_LEN),
            ..Default::default()
        },
        &BlockingSpawner::new(1),
    )
    .await?;
    let torrent_bytes = torrent.as_bytes()?;

    let server_session = Session::new_with_opts(
        files.path().into(),
        crate::SessionOptions {
            dht: None,
            peer_id: Some(TestPeerMetadata::good().as_peer_id()),
            persistence: None,
            listen: Some(crate::listen::ListenerOptions {
                listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .context("error creating server session")?;
    timeout(
        Duration::from_secs(30),
        server_session
            .add_torrent(
                AddTorrent::from_bytes(torrent_bytes.clone()),
                Some(crate::AddTorrentOptions {
                    paused: false,
                    output_folder: Some(files.path().to_str().unwrap().to_owned()),
                    overwrite: true,
                    ..Default::default()
                }),
            )
            .await?
            .into_handle()
            .context("expected a handle")?
            .wait_until_completed(),
    )
    .await?
    .context("error adding torrent to server")?;
    let peer = server_session
        .listen_addr()
        .context("expected listen_addr to be set")?;

    let storage = RefusingCommitStorageFactory::default();
    let client_dir = TempDir::with_prefix("test_refused_commit_client")?;
    let client_session = Session::new_with_opts(
        client_dir.path().into(),
        crate::SessionOptions {
            dht: None,
            persistence: None,
            peer_id: Some(TestPeerMetadata::good().as_peer_id()),
            ..Default::default()
        },
    )
    .await?;
    let handle = client_session
        .add_torrent(
            AddTorrent::from_bytes(torrent_bytes),
            Some(crate::AddTorrentOptions {
                paused: false,
                initial_peers: Some(vec![peer]),
                storage_factory: Some(storage.clone().boxed()),
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .context("expected a handle")?;

    // The first piece to pass its hash check is refused, and that is the end of it.
    timeout(Duration::from_secs(30), async {
        loop {
            if handle.with_state(|s| matches!(s, crate::ManagedTorrentState::Error(_))) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("the torrent didn't stop: a refused commit was swallowed")?;

    let stats = handle.stats();
    assert!(!stats.finished);
    assert_eq!(stats.progress_bytes, 0);
    let error = stats.error.context("expected the torrent's error")?;
    assert!(error.contains("no space left on device"), "{error}");
    assert!(storage.attempts.load(std::sync::atomic::Ordering::Relaxed) >= 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_e2e_refused_commit_is_fatal() -> anyhow::Result<()> {
    timeout(Duration::from_secs(120), e2e_refused_commit_is_fatal()).await?
}
