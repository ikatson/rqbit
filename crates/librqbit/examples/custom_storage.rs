use std::time::Duration;

use anyhow::Context;
use librqbit::{
    storage::{StorageFactory, TorrentStorage},
    FileInfos, ManagedTorrentInfo, SessionOptions,
};
use memmap2::{MmapMut, MmapOptions};
use parking_lot::RwLock;
use tracing::info;

struct MmapStorageFactory {}

struct MmapStorage {
    mmap: RwLock<MmapMut>,
    file_infos: FileInfos,
}

impl StorageFactory for MmapStorageFactory {
    fn init_storage(
        &self,
        info: &ManagedTorrentInfo,
    ) -> anyhow::Result<Box<dyn librqbit::storage::TorrentStorage>> {
        Ok(Box::new(MmapStorage {
            mmap: RwLock::new(
                MmapOptions::new()
                    .len(info.lengths.total_length().try_into()?)
                    .map_anon()?,
            ),
            file_infos: info.file_infos.clone(),
        }))
    }
}

impl TorrentStorage for MmapStorage {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        let start: usize = (self.file_infos[file_id].offset_in_torrent + offset).try_into()?;
        let end = start + buf.len();
        buf.copy_from_slice(self.mmap.read().get(start..end).context("bad range")?);
        Ok(())
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        let start: usize = (self.file_infos[file_id].offset_in_torrent + offset).try_into()?;
        let end = start + buf.len();
        let mut g = self.mmap.write();
        let target = g.get_mut(start..end).context("bad range")?;
        target.copy_from_slice(buf);
        Ok(())
    }

    fn remove_file(&self, _file_id: usize, _filename: &std::path::Path) -> anyhow::Result<()> {
        Ok(())
    }

    fn ensure_file_length(&self, _file_id: usize, _length: u64) -> anyhow::Result<()> {
        Ok(())
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
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
            persistence: false,
            listen_port_range: None,
            enable_upnp_port_forwarding: false,
            ..Default::default()
        },
    )
    .await?;
    let handle = s
        .add_torrent(
            librqbit::AddTorrent::TorrentFileBytes(
                include_bytes!("../resources/ubuntu-21.04-live-server-amd64.iso.torrent").into(),
            ),
            Some(librqbit::AddTorrentOptions {
                storage_factory: Some(Box::new(MmapStorageFactory {})),
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
