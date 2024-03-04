use std::{
    borrow::Cow,
    fs::Permissions,
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

use anyhow::bail;
use futures::{
    stream::{FuturesOrdered, FuturesUnordered},
    StreamExt,
};
use tokio::spawn;

use crate::{
    create_torrent,
    tests::test_util::{create_default_random_dir_with_torrents, NamedTempDir},
    AddTorrentOptions,
};

#[tokio::test]
async fn test_e2e() {
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

    for _ in 0..num_servers {
        let torrent_file_bytes = torrent_file_bytes.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let tempdir = tempdir.name().to_owned();
        spawn(async move {
            // 2. Start N servers that are serving that torrent, and return their IP:port combos.
            //    Disable DHT on each.
            let session = crate::Session::new_with_opts(
                std::env::temp_dir().join("does_not_exist"),
                crate::SessionOptions {
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
            let h = match handle {
                crate::AddTorrentResponse::AlreadyManaged(_, _) => panic!("bug"),
                crate::AddTorrentResponse::ListOnly(_) => panic!("bug"),
                crate::AddTorrentResponse::Added(_, h) => h,
            };
            let mut interval = tokio::time::interval(Duration::from_secs(1));
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
            tx.send(SocketAddr::new(
                std::net::IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                session.tcp_listen_port().unwrap(),
            ))
        });
        futs.push(tokio::time::timeout(Duration::from_secs(10), rx));
    }

    let mut peers = Vec::new();
    while let Some(addr) = futs.next().await {
        peers.push(addr.unwrap().unwrap());
    }

    dbg!(peers);

    // 3. Start a client with the initial peers, and download the file.

    // 4. After downloading, recheck its integrity.
}
