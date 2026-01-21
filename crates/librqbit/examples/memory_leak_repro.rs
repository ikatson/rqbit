use std::{io::Write, net::Ipv4Addr, path::Path, time::Duration};

use librqbit::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, ListenerOptions, Session, SessionOptions,
    create_torrent,
    spawn_utils::BlockingSpawner,
    storage::{BoxStorageFactory, StorageFactoryExt},
};
use rand::{RngCore, SeedableRng};
use tempfile::TempDir;
use tracing::{info, info_span};

fn create_new_file_with_random_content(path: &Path, mut size: usize) {
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .unwrap();

    info!(?path, "creating temp file");

    const BUF_SIZE: usize = 8192 * 16;
    let mut rng = rand::rngs::SmallRng::from_os_rng();
    let mut write_buf = [0; BUF_SIZE];
    while size > 0 {
        rng.fill_bytes(&mut write_buf[..]);
        let written = file.write(&write_buf[..size.min(BUF_SIZE)]).unwrap();
        size -= written;
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let td = TempDir::new()?;
    let filename = td.path().join("file");
    create_new_file_with_random_content(&filename, 1024 * 1024);
    let torrent_bytes = create_torrent(
        &filename,
        CreateTorrentOptions::default(),
        &BlockingSpawner::new(1),
    )
    .await?
    .as_bytes()?;

    let server = Session::new_with_opts(
        td.path().join("server"),
        SessionOptions {
            disable_dht: true,
            persistence: None,
            listen: Some(ListenerOptions {
                mode: librqbit::ListenerMode::TcpOnly,
                listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
                enable_upnp_port_forwarding: false,
                ..Default::default()
            }),
            root_span: Some(info_span!("server")),
            disable_local_service_discovery: true,
            ..Default::default()
        },
    )
    .await?;

    server
        .add_torrent(
            AddTorrent::from_bytes(torrent_bytes.clone()),
            Some(AddTorrentOptions {
                overwrite: true,
                output_folder: Some(td.path().to_str().unwrap().to_owned()),
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .unwrap()
        .wait_until_completed()
        .await?;

    let server_addr = server.listen_addr().unwrap();

    let tasks = (0..1).map(|client_id| {
        let torrent_bytes = torrent_bytes.clone();
        let root = td.path().join(client_id.to_string());
        async move {
            use librqbit::storage::examples::inmemory::InMemoryExampleStorageFactory;
            let client = Session::new_with_opts(
                root,
                SessionOptions {
                    disable_dht: true,
                    persistence: None,
                    listen: Some(ListenerOptions {
                        mode: librqbit::ListenerMode::TcpOnly,
                        listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
                        enable_upnp_port_forwarding: false,
                        ..Default::default()
                    }),
                    root_span: Some(info_span!("client", client_id)),
                    disable_local_service_discovery: true,
                    // save disk
                    default_storage_factory: Some(InMemoryExampleStorageFactory {}.boxed()),
                    ..Default::default()
                },
            )
            .await?;

            let mut it = 1;
            loop {
                tracing::info!(iteration = it, "client iteration");
                it += 1;
                let handle = client
                    .add_torrent(
                        AddTorrent::TorrentFileBytes(torrent_bytes.clone()),
                        Some(AddTorrentOptions {
                            initial_peers: Some(vec![server_addr]),
                            ..Default::default()
                        }),
                    )
                    .await?
                    .into_handle()
                    .unwrap();
                tokio::time::timeout(Duration::from_secs(30), handle.wait_until_completed())
                    .await??;

                client.delete(handle.id().into(), true).await?;
            }

            #[allow(unreachable_code)]
            Ok::<_, anyhow::Error>(())
        }
    });

    futures::future::try_join_all(tasks).await?;

    Ok(())
}
