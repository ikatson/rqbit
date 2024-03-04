use std::{
    borrow::Cow,
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

use anyhow::bail;
use futures::{stream::FuturesUnordered, StreamExt};
use tokio::spawn;
use tracing::{error_span, info, Instrument};

use crate::{
    create_torrent, tests::test_util::create_default_random_dir_with_torrents, AddTorrentOptions,
    AddTorrentResponse, Session, SessionOptions,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_e2e() {
    let _ = tracing_subscriber::fmt::try_init();

    // 1. Create a torrent
    // Ideally (for a more complicated test) with N files, and at least N pieces that span 2 files.

    let piece_length = 16384u32; // TODO: figure out if this should be multiple of chunk size or not
    let file_length = piece_length * 3 + 1;
    let num_files = 64;

    let tempdir = create_default_random_dir_with_torrents(num_files, file_length as usize);
    let torrent_file = create_torrent(
        dbg!(tempdir.name()),
        crate::CreateTorrentOptions {
            piece_length: Some(piece_length),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let num_servers = 32;

    let torrent_file_bytes = torrent_file.as_bytes().unwrap();
    let mut futs = FuturesUnordered::new();

    for i in 0..num_servers {
        let torrent_file_bytes = torrent_file_bytes.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let tempdir = tempdir.name().to_owned();
        spawn(
            async move {
                // 2. Start N servers that are serving that torrent, and return their IP:port combos.
                //    Disable DHT on each.
                let session = crate::Session::new_with_opts(
                    std::env::temp_dir().join("does_not_exist"),
                    SessionOptions {
                        disable_dht: true,
                        disable_dht_persistence: true,
                        dht_config: None,
                        persistence: false,
                        persistence_filename: None,
                        peer_id: None,
                        peer_opts: None,
                        listen_port_range: Some(15100..15200),
                        enable_upnp_port_forwarding: false,
                    },
                )
                .await
                .unwrap();

                info!("started session");

                let handle = session
                    .add_torrent(
                        crate::AddTorrent::TorrentFileBytes(Cow::Owned(torrent_file_bytes)),
                        Some(AddTorrentOptions {
                            overwrite: true,
                            output_folder: Some(tempdir.to_str().unwrap().to_owned()),
                            ..Default::default()
                        }),
                    )
                    .await
                    .unwrap();
                let h = handle.into_handle().unwrap();
                let mut interval = tokio::time::interval(Duration::from_millis(100));

                info!("added torrent");
                loop {
                    interval.tick().await;
                    let is_live = h
                        .with_state(|s| match s {
                            crate::ManagedTorrentState::Initializing(_) => Ok(false),
                            crate::ManagedTorrentState::Live(l) => {
                                if !l.is_finished() {
                                    bail!("torrent went live, but expected it to be finished");
                                }
                                Ok(true)
                            }
                            _ => bail!("broken state"),
                        })
                        .unwrap();
                    if is_live {
                        break;
                    }
                }
                info!("torrent is live");
                tx.send(SocketAddr::new(
                    std::net::IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                    session.tcp_listen_port().unwrap(),
                ))
            }
            .instrument(error_span!("server", server = i)),
        );
        futs.push(tokio::time::timeout(Duration::from_secs(10), rx));
    }

    let mut peers = Vec::new();
    while let Some(addr) = futs.next().await {
        peers.push(addr.unwrap().unwrap());
    }

    info!("started all servers, starting client");

    // 3. Start a client with the initial peers, and download the file.
    let outdir = tempdir.name().join("output");
    let session = Session::new_with_opts(
        outdir,
        SessionOptions {
            disable_dht: true,
            disable_dht_persistence: true,
            dht_config: None,
            persistence: false,
            persistence_filename: None,
            listen_port_range: None,
            enable_upnp_port_forwarding: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    info!("started client session");

    let (id, handle) = {
        let r = session
            .add_torrent(
                crate::AddTorrent::TorrentFileBytes(Cow::Owned(torrent_file_bytes.clone())),
                Some(AddTorrentOptions {
                    initial_peers: Some(peers),
                    overwrite: false,
                    ..Default::default()
                }),
            )
            .await
            .unwrap();

        match r {
            AddTorrentResponse::AlreadyManaged(_, _) => todo!(),
            AddTorrentResponse::ListOnly(_) => todo!(),
            AddTorrentResponse::Added(id, h) => (id, h),
        }
    };

    info!("added handle");

    tokio::time::timeout(Duration::from_secs(10), handle.wait_until_completed())
        .await
        .unwrap()
        .unwrap();

    info!("handle is completed");
    session.delete(id, false).unwrap();

    info!("deleted handle");

    // 4. After downloading, recheck its integrity.
    let handle = session
        .add_torrent(
            crate::AddTorrent::TorrentFileBytes(Cow::Owned(torrent_file_bytes)),
            Some(AddTorrentOptions {
                paused: true,
                overwrite: true,
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .into_handle()
        .unwrap();

    info!("re-added handle");

    let mut interval = tokio::time::interval(Duration::from_millis(100));
    for _ in 0..100 {
        interval.tick().await;
        let b = handle
            .with_state(|s| match s {
                crate::ManagedTorrentState::Initializing(_) => Ok(false),
                crate::ManagedTorrentState::Paused(p) => {
                    assert_eq!(p.needed_bytes, 0);
                    Ok(true)
                }
                _ => bail!("bugged state"),
            })
            .unwrap();
        if b {
            break;
        }
    }

    info!("all good");
}
