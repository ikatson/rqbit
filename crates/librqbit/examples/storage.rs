use librqbit::{
    storage::{StorageFactory, TorrentStorage},
    ManagedTorrentInfo, SessionOptions,
};

struct DummyStorage {}

impl StorageFactory for DummyStorage {
    fn init_storage(
        &self,
        _info: &ManagedTorrentInfo,
    ) -> anyhow::Result<Box<dyn librqbit::storage::TorrentStorage>> {
        Ok(Box::new(DummyStorage {}))
    }
}

impl TorrentStorage for DummyStorage {
    fn pread_exact(&self, _file_id: usize, _offset: u64, _buf: &mut [u8]) -> anyhow::Result<()> {
        anyhow::bail!("pread_exact")
    }

    fn pwrite_all(&self, _file_id: usize, _offset: u64, _buf: &[u8]) -> anyhow::Result<()> {
        anyhow::bail!("pwrite_all")
    }

    fn remove_file(&self, _file_id: usize, _filename: &std::path::Path) -> anyhow::Result<()> {
        anyhow::bail!("remove_file")
    }

    fn ensure_file_length(&self, _file_id: usize, _length: u64) -> anyhow::Result<()> {
        anyhow::bail!("ensure_file_length")
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        Ok(Box::new(Self {}))
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
        "/does-not-matter".into(),
        SessionOptions {
            disable_dht: true,
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
                storage_factory: Some(Box::new(DummyStorage {})),
                paused: true,
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .unwrap();
    handle.wait_until_initialized().await?;
    Ok(())
}
