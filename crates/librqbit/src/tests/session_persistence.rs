// What session persistence needs from a storage, end to end.
//
// The persisted record is the torrent, an output folder, a file selection and a paused
// flag. What isn't in it is the storage: a restart replays the record through
// add_torrent, which builds the session's default storage from the output folder and the
// file selection, and beside the record there is a have-bitfield the previous run wrote.
// So what makes a torrent persistable is a promise its storage makes - see
// StorageFactory::ensure_persistable - and not which storage it happens to be.

use std::{
    net::Ipv4Addr,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Context;
use tempfile::TempDir;
use tokio::{io::AsyncReadExt, time::timeout};

use crate::{
    AddTorrent, CreateTorrentOptions, ManagedTorrentShared, Session, TorrentMetadata,
    create_torrent,
    spawn_utils::BlockingSpawner,
    storage::{
        BoxStorageFactory, StorageFactory, StorageFactoryExt, TorrentStorage,
        filesystem::OurFileExt,
    },
    tests::test_util::{TestPeerMetadata, setup_test_logging},
    type_aliases::{BF, FileInfos},
};

use super::test_util::create_default_random_dir_with_torrents;

const PIECE_LEN: u32 = 16384;
const TOTAL_PIECES: u32 = 8;
const FILE_SIZE: usize = (PIECE_LEN * TOTAL_PIECES) as usize;

// A storage that is not the filesystem one: it keeps the whole torrent as a single flat
// blob in a file of its own choosing, which is nothing like the torrent's own layout.
//
// It can promise what persistence needs all the same. The blob is where the factory says
// it is, so the session's default factory finds the same data in the next process; and
// the bytes are on a disk, so the have-bitfield the previous run left is still true when
// that process reads it.
#[derive(Clone)]
struct BlobStorageFactory {
    filename: PathBuf,
}

impl StorageFactory for BlobStorageFactory {
    type Storage = BlobStorage;

    fn create(
        &self,
        _shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<BlobStorage> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.filename)
            .with_context(|| format!("error opening {:?}", self.filename))?;
        file.set_len(metadata.lengths().total_length())?;
        Ok(BlobStorage {
            file,
            file_infos: metadata.file_infos.clone(),
        })
    }

    fn ensure_persistable(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn clone_box(&self) -> BoxStorageFactory {
        self.clone().boxed()
    }
}

struct BlobStorage {
    file: std::fs::File,
    file_infos: FileInfos,
}

impl BlobStorage {
    fn offset(&self, file_id: usize, offset: u64) -> anyhow::Result<u64> {
        Ok(self
            .file_infos
            .get(file_id)
            .context("no such file")?
            .offset_in_torrent
            + offset)
    }
}

impl TorrentStorage for BlobStorage {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        self.file.pread_exact(self.offset(file_id, offset)?, buf)
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        self.file.pwrite_all(self.offset(file_id, offset)?, buf)
    }

    fn remove_file(&self, _file_id: usize, _filename: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    fn ensure_file_length(&self, _file_id: usize, _length: u64) -> anyhow::Result<()> {
        Ok(())
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        Ok(Box::new(BlobStorage {
            file: self.file.try_clone()?,
            file_infos: self.file_infos.clone(),
        }))
    }

    fn init(
        &mut self,
        _shared: &ManagedTorrentShared,
        _metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn remove_directory_if_empty(&self, _path: &Path) -> anyhow::Result<()> {
        Ok(())
    }
}

// A wrapper that hands out someone else's storage without passing on what it promises -
// which is every storage that hasn't thought about persistence, the default being no.
#[derive(Clone)]
struct NoPromises<U> {
    underlying_factory: U,
}

impl<U: StorageFactory + Clone> StorageFactory for NoPromises<U> {
    type Storage = U::Storage;

    fn create(
        &self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<Self::Storage> {
        self.underlying_factory.create(shared, metadata)
    }

    fn clone_box(&self) -> BoxStorageFactory {
        self.clone().boxed()
    }
}

// A session that has the whole torrent and will serve it, plus the torrent file and the
// address to connect to.
async fn seeding_server(
    prefix: &str,
) -> anyhow::Result<(
    TempDir,
    Vec<u8>,
    std::sync::Arc<Session>,
    std::net::SocketAddr,
)> {
    let files = create_default_random_dir_with_torrents(1, FILE_SIZE, Some(prefix));
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
    Ok((files, torrent_bytes.to_vec(), server_session, peer))
}

// A client session with persistence and fastresume on, so that a second one over the same
// folders is a restart.
fn session_opts(
    persistence_folder: &Path,
    storage_factory: Option<BoxStorageFactory>,
) -> crate::SessionOptions {
    crate::SessionOptions {
        dht: None,
        persistence: Some(crate::SessionPersistenceConfig::Json {
            folder: Some(persistence_folder.to_owned()),
        }),
        fastresume: true,
        disable_local_service_discovery: true,
        peer_id: Some(TestPeerMetadata::good().as_peer_id()),
        default_storage_factory: storage_factory,
        ..Default::default()
    }
}

// The have-bitfield the previous run left behind, which is the claim a restart starts
// from. Written when the session is dropped, so this waits for it.
async fn resume_data(persistence_folder: &Path) -> anyhow::Result<BF> {
    let filename = std::fs::read_dir(persistence_folder)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|e| e == "bitv"))
        .context("expected a .bitv file in the persistence folder")?;
    let mut last = BF::default();
    for _ in 0..100 {
        last = BF::from_boxed_slice(std::fs::read(&filename)?.into_boxed_slice());
        if last.count_ones() == TOTAL_PIECES as usize {
            return Ok(last);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(last)
}

// Read the file back through the torrent, which is what a consumer of it does.
async fn read_back(handle: std::sync::Arc<crate::ManagedTorrent>) -> anyhow::Result<Vec<u8>> {
    let mut stream = handle.stream(0).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    Ok(buf)
}

// Download the whole torrent into a fresh session, then restart that session over the
// same persistence folder and hand back the torrent as it came out of the database.
async fn download_then_restart(
    prefix: &str,
    storage: impl Fn() -> Option<BoxStorageFactory>,
) -> anyhow::Result<(TempDir, Vec<u8>, PathBuf, std::sync::Arc<Session>)> {
    let (files, torrent_bytes, _server_session, peer) = seeding_server(prefix).await?;
    let orig_content = std::fs::read(files.path().join("0.data")).unwrap();

    let dir = TempDir::with_prefix(format!("{prefix}_client"))?;
    let output_folder = dir.path().join("out");
    let persistence_folder = dir.path().join("session");

    let session = Session::new_with_opts(
        output_folder.clone(),
        session_opts(&persistence_folder, storage()),
    )
    .await?;
    let handle = session
        .add_torrent(
            AddTorrent::from_bytes(torrent_bytes.clone()),
            Some(crate::AddTorrentOptions {
                paused: false,
                initial_peers: Some(vec![peer]),
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .context("expected a handle")?;
    timeout(Duration::from_secs(30), handle.wait_until_completed()).await??;
    assert_eq!(read_back(handle.clone()).await?, orig_content);

    // The record is on disk, and it says where the data is - which, for a storage that
    // makes the promise, is all a restart needs.
    let db: serde_json::Value =
        serde_json::from_slice(&std::fs::read(persistence_folder.join("session.json"))?)?;
    assert_eq!(
        db["torrents"]["0"]["output_folder"].as_str().map(Path::new),
        Some(output_folder.as_path())
    );

    drop(handle);
    drop(session);

    // The have-bitfield claims the whole torrent, which is what the restart starts from.
    assert_eq!(
        resume_data(&persistence_folder).await?.count_ones(),
        TOTAL_PIECES as usize
    );

    let session = Session::new_with_opts(
        output_folder.clone(),
        session_opts(&persistence_folder, storage()),
    )
    .await?;
    Ok((dir, orig_content, output_folder, session))
}

// The default path, asserted rather than assumed: the filesystem storage promises what
// persistence needs, so a torrent using it is written to the database and comes back
// whole.
async fn the_filesystem_storage_is_persisted_and_restored() -> anyhow::Result<()> {
    setup_test_logging();
    // No storage factory: this is what everyone gets.
    let (_dir, orig_content, output_folder, session) =
        download_then_restart("test_persistence_fs", || None).await?;

    assert_eq!(
        std::fs::read(output_folder.join("0.data")).unwrap(),
        orig_content
    );
    let handle = session
        .get(crate::api::TorrentIdOrHash::Id(0))
        .context("expected the torrent to be restored from persistence")?;
    timeout(Duration::from_secs(30), handle.wait_until_completed()).await??;
    handle.with_chunk_tracker(|ct| {
        assert!(ct.get_have_pieces().as_slice()[..TOTAL_PIECES as usize].all());
    })?;
    assert_eq!(read_back(handle).await?, orig_content);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_the_filesystem_storage_is_persisted_and_restored() -> anyhow::Result<()> {
    timeout(
        Duration::from_secs(120),
        the_filesystem_storage_is_persisted_and_restored(),
    )
    .await?
}

// A storage that is not the filesystem one is persisted and restored just as well, on the
// strength of the promise it makes. Nothing about the record changes: it still carries an
// output folder this storage puts nothing in, and the restart still rebuilds the storage
// from the session's default factory - which is the whole of what the promise is about.
async fn a_non_filesystem_storage_is_persisted_and_restored() -> anyhow::Result<()> {
    setup_test_logging();
    let blob = TempDir::with_prefix("test_persistence_blob_storage")?;
    let filename = blob.path().join("blob.bin");
    // A new factory for each session, pointed at the same blob: nothing is carried over
    // in memory, exactly as it wouldn't be across a real restart.
    let storage = || {
        Some(
            BlobStorageFactory {
                filename: filename.clone(),
            }
            .boxed(),
        )
    };

    let (_dir, orig_content, output_folder, session) =
        download_then_restart("test_persistence_blob", storage).await?;

    // The data is in the blob, and the output folder in the record names nothing.
    assert_eq!(std::fs::read(&filename)?, orig_content);
    assert!(!output_folder.join("0.data").exists());

    let handle = session
        .get(crate::api::TorrentIdOrHash::Id(0))
        .context("expected the torrent to be restored from persistence")?;
    timeout(Duration::from_secs(30), handle.wait_until_completed()).await??;
    handle.with_chunk_tracker(|ct| {
        assert!(ct.get_have_pieces().as_slice()[..TOTAL_PIECES as usize].all());
    })?;
    assert_eq!(read_back(handle).await?, orig_content);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_non_filesystem_storage_is_persisted_and_restored() -> anyhow::Result<()> {
    timeout(
        Duration::from_secs(120),
        a_non_filesystem_storage_is_persisted_and_restored(),
    )
    .await?
}

// A storage that doesn't promise is refused when the torrent is added, naming what didn't
// promise it - not at the next restart, when the record would be there and the data
// wouldn't.
async fn a_storage_that_cant_promise_persistence_is_refused_at_add_time() -> anyhow::Result<()> {
    setup_test_logging();
    let files = create_default_random_dir_with_torrents(1, FILE_SIZE, Some("test_persistence_bad"));
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

    let dir = TempDir::with_prefix("test_persistence_bad_client")?;
    let persistence_folder = dir.path().join("session");
    let session = Session::new_with_opts(
        dir.path().join("out"),
        session_opts(
            &persistence_folder,
            Some(
                NoPromises {
                    underlying_factory: BlobStorageFactory {
                        filename: dir.path().join("blob.bin"),
                    },
                }
                .boxed(),
            ),
        ),
    )
    .await?;

    let err = session
        .add_torrent(
            AddTorrent::from_bytes(torrent.as_bytes()?),
            Some(crate::AddTorrentOptions {
                paused: true,
                ..Default::default()
            }),
        )
        .await
        .err()
        .context("expected adding a torrent with this storage to fail")?;
    let err = format!("{err:#}");
    assert!(err.contains("NoPromises"), "{err}");
    assert!(err.contains("ensure_persistable"), "{err}");

    // Loudly, and at add time: nothing is left in the session, and nothing was written
    // for a restart to find.
    assert!(session.get(crate::api::TorrentIdOrHash::Id(0)).is_none());
    assert!(!persistence_folder.join("session.json").exists());

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_storage_that_cant_promise_persistence_is_refused_at_add_time() -> anyhow::Result<()>
{
    timeout(
        Duration::from_secs(120),
        a_storage_that_cant_promise_persistence_is_refused_at_add_time(),
    )
    .await?
}
