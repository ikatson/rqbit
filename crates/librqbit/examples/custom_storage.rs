use bytes::Bytes;
use librqbit::{
    storage::{StorageFactory, StorageFactoryExt, TorrentStorage},
    SessionOptions,
};
use tracing::info;

use std::time::Duration;

#[derive(Default, Clone, Copy)]
struct CustomStorageFactory {
    _some_state_used_to_create_per_torrent_storage: (),
}

#[derive(Default, Clone, Copy)]
struct CustomStorage {
    _some_state_for_per_torrent_storage: (),
}

impl StorageFactory for CustomStorageFactory {
    type Storage = CustomStorage;

    fn create(
        &self,
        _: &librqbit::ManagedTorrentShared,
        _: &librqbit::TorrentMetadata,
    ) -> anyhow::Result<Self::Storage> {
        Ok(CustomStorage::default())
    }

    fn clone_box(&self) -> librqbit::storage::BoxStorageFactory {
        self.boxed()
    }
}

impl TorrentStorage for CustomStorage {
    fn pread_exact(&self, _file_id: usize, _offset: u64, _buf: &mut [u8]) -> anyhow::Result<()> {
        anyhow::bail!("not implemented")
    }

    fn pwrite_all(&self, _file_id: usize, _offset: u64, _buf: &[u8]) -> anyhow::Result<()> {
        anyhow::bail!("not implemented")
    }

    fn remove_file(&self, _file_id: usize, _filename: &std::path::Path) -> anyhow::Result<()> {
        anyhow::bail!("not implemented")
    }

    fn ensure_file_length(&self, _file_id: usize, _length: u64) -> anyhow::Result<()> {
        anyhow::bail!("not implemented")
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        anyhow::bail!("not implemented")
    }

    fn remove_directory_if_empty(&self, _path: &std::path::Path) -> anyhow::Result<()> {
        anyhow::bail!("not implemented")
    }

    fn init(
        &mut self,
        _meta: &librqbit::ManagedTorrentShared,
        _: &librqbit::TorrentMetadata,
    ) -> anyhow::Result<()> {
        anyhow::bail!("not implemented")
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Output logs to console.
    match std::env::var("RUST_LOG") {
        Ok(_) => {}
        Err(_) => std::env::set_var("RUST_LOG", "info"),
    }
    tracing_subscriber::fmt::init();
    let s = librqbit::Session::new_with_opts(
        Default::default(),
        SessionOptions {
            disable_dht_persistence: true,
            persistence: None,
            ..Default::default()
        },
    )
    .await?;
    let handle = s
        .add_torrent(
            librqbit::AddTorrent::TorrentFileBytes(Bytes::from_static(include_bytes!(
                "../resources/ubuntu-21.04-live-server-amd64.iso.torrent"
            ))),
            Some(librqbit::AddTorrentOptions {
                storage_factory: Some(CustomStorageFactory::default().boxed()),
                paused: false,
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .unwrap();
    tokio::spawn({
        let h = handle.clone();
        async move {
            loop {
                info!("{}", h.stats());
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    });
    handle.wait_until_completed().await?;
    Ok(())
}
