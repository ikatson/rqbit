use std::{
    path::Path,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

use anyhow::Context;
use tokio::time::timeout;

use crate::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, ManagedTorrentShared, Session,
    SessionOptions, TorrentMetadata, TorrentStatsState, create_torrent,
    spawn_utils::BlockingSpawner,
    storage::{
        BoxStorageFactory, StorageFactory, StorageFactoryExt, TorrentStorage,
        filesystem::FilesystemStorageFactory,
    },
    tests::test_util::{create_new_file_with_random_content, setup_test_logging},
};

#[derive(Default)]
struct GateState {
    entered: bool,
    released: bool,
}

#[derive(Default)]
struct Gate {
    state: Mutex<GateState>,
    cond: Condvar,
}

impl Gate {
    // Called on the initial check's blocking thread once every piece was hashed.
    fn enter_and_wait(&self) {
        let mut g = self.state.lock().unwrap();
        g.entered = true;
        self.cond.notify_all();
        while !g.released {
            g = self.cond.wait(g).unwrap();
        }
    }

    fn wait_entered(&self) {
        let mut g = self.state.lock().unwrap();
        while !g.entered {
            g = self.cond.wait(g).unwrap();
        }
    }

    fn release(&self) {
        self.state.lock().unwrap().released = true;
        self.cond.notify_all();
    }
}

// Holds the end of the initial check (after the piece loop, before the torrent leaves
// Initializing) so a pause can be requested exactly there.
#[derive(Clone)]
struct GatedFactory {
    inner: FilesystemStorageFactory,
    gate: Arc<Gate>,
}

struct GatedStorage {
    inner: Box<dyn TorrentStorage>,
    gate: Arc<Gate>,
}

impl StorageFactory for GatedFactory {
    type Storage = GatedStorage;

    fn create(
        &self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<GatedStorage> {
        Ok(GatedStorage {
            inner: Box::new(self.inner.create(shared, metadata)?),
            gate: self.gate.clone(),
        })
    }

    fn clone_box(&self) -> BoxStorageFactory {
        self.clone().boxed()
    }
}

impl TorrentStorage for GatedStorage {
    fn init(
        &mut self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        self.inner.init(shared, metadata)
    }

    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
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
        self.gate.enter_and_wait();
        self.inner.ensure_file_length(file_id, length)
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        self.inner.take()
    }
}

// A pause requested after the initial check hashed its last piece can no longer abort
// the check; it must still be honored when the check finishes instead of the torrent
// going live while the caller believes it's paused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_pause_requested_after_last_piece_lands_paused() -> anyhow::Result<()> {
    timeout(Duration::from_secs(30), pause_requested_after_last_piece())
        .await
        .context("test timed out")?
}

async fn pause_requested_after_last_piece() -> anyhow::Result<()> {
    setup_test_logging();
    let src = tempfile::TempDir::with_prefix("rqbit_init_pause_src")?;
    let out = tempfile::TempDir::with_prefix("rqbit_init_pause_out")?;
    let file = src.path().join("payload.bin");
    create_new_file_with_random_content(&file, 256 * 1024);
    let torrent = create_torrent(
        &file,
        CreateTorrentOptions::default(),
        &BlockingSpawner::new(1),
    )
    .await?
    .as_bytes()?;

    let session = Session::new_with_opts(
        out.path().to_owned(),
        SessionOptions {
            dht: None,
            listen: None,
            disable_trackers: true,
            disable_local_service_discovery: true,
            ..Default::default()
        },
    )
    .await?;

    let gate = Arc::new(Gate::default());
    let handle = session
        .add_torrent(
            AddTorrent::TorrentFileBytes(torrent),
            Some(AddTorrentOptions {
                output_folder: Some(out.path().to_string_lossy().into_owned()),
                overwrite: true,
                storage_factory: Some(
                    GatedFactory {
                        inner: FilesystemStorageFactory::default(),
                        gate: gate.clone(),
                    }
                    .boxed(),
                ),
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .context("no handle")?;

    let waiter = gate.clone();
    tokio::task::spawn_blocking(move || waiter.wait_entered()).await?;
    assert!(matches!(
        handle.stats().state,
        TorrentStatsState::Initializing { .. }
    ));
    session.pause(&handle).await?;
    gate.release();

    timeout(Duration::from_secs(10), handle.wait_until_initialized()).await??;
    assert!(
        matches!(handle.stats().state, TorrentStatsState::Paused),
        "late pause was lost: {:?}",
        handle.stats().state
    );
    assert!(handle.is_paused());

    session.unpause(&handle).await?;
    assert!(matches!(handle.stats().state, TorrentStatsState::Live));
    assert!(!handle.is_paused());
    Ok(())
}
