# librqbit

A torrent library 100% written in rust

## Basic example
This is a simple on how to use this library  
This program will just download a simple torrent file with a Magnet link

```rust
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use librqbit::{http_api_client, Magnet};
use librqbit::session::{AddTorrentResponse, ListOnlyResponse, ManagedTorrentState, Session};
use librqbit::spawn_utils::BlockingSpawner;
use size_format::SizeFormatterBinary as SF;
use tokio::spawn;

const MAGNET_LINK: &str = "magnet:?..."; // Put your magnet link here

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>>{

    let spawner = BlockingSpawner::new(false);

    // This function will print the torrent properties every 1s
    let stats_printer = |session: Arc<Session>| async move {
        loop {
            session.with_torrents(|torrents| {
                for (idx, torrent) in torrents.iter().enumerate() {
                    match &torrent.state {
                        ManagedTorrentState::Initializing => {
                            println!("[{}] initializing", idx);
                        },
                        ManagedTorrentState::Running(handle) => {
                            let peer_stats = handle.torrent_state().peer_stats_snapshot();
                            let stats = handle.torrent_state().stats_snapshot();
                            let speed = handle.speed_estimator();
                            let total = stats.total_bytes;
                            let progress = stats.total_bytes - stats.remaining_bytes;
                            let downloaded_pct = if stats.remaining_bytes == 0 {
                                100f64
                            } else {
                                (progress as f64 / total as f64) * 100f64
                            };
                            println!(
                                "[{}]: {:.2}% ({:.2}), down speed {:.2} MiB/s, fetched {}, remaining {:.2} of {:.2}, uploaded {:.2}, peers: {{live: {}, connecting: {}, queued: {}, seen: {}}}",
                                idx,
                                downloaded_pct,
                                SF::new(progress),
                                speed.download_mbps(),
                                SF::new(stats.fetched_bytes),
                                SF::new(stats.remaining_bytes),
                                SF::new(total),
                                SF::new(stats.uploaded_bytes),
                                peer_stats.live,
                                peer_stats.connecting,
                                peer_stats.queued,
                                peer_stats.seen,
                            );
                        },
                    }
                }
            });
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    };

    // Create the torrent session, one session can have more than one torrent at the same time
    let session = Arc::new(
        Session::new(PathBuf::from("C:\\Anime"), spawner).await?
    );

    // Spawn the properties printer function
    spawn(stats_printer(session.clone()));

    // Add the magnet link to the torrent session, you don't have to specify if it's an url or magnet,
    // the library will recognize automatically for you
    let handle = match session.add_torrent(MAGNET_LINK, None).await {
        Ok(v) => match v {
            AddTorrentResponse::AlreadyManaged(handle) => {
                println!(
                    "torrent {:?} is already managed, downloaded to {:?}",
                    handle.info_hash, handle.output_folder
                );
                Err(())
            }
            AddTorrentResponse::ListOnly(ListOnlyResponse {
                                             info_hash: _,
                                             info,
                                             only_files,
                                         }) => {
                for (idx, (filename, len)) in
                info.iter_filenames_and_lengths()?.enumerate()
                {
                    let included = match &only_files {
                        Some(files) => files.contains(&idx),
                        None => true,
                    };
                    println!(
                        "File {}, size {}{}",
                        filename.to_string()?,
                        SF::new(len),
                        if included { "" } else { ", will skip" }
                    )
                }
                Err(())
            }
            AddTorrentResponse::Added(handle) => {
                Ok(handle)
            }
        }
        Err(err) => {
            eprintln!("error adding {}: {:?}", MAGNET_LINK, err);
            Err(())
        }
    };

    // Wait until the session complete the torrent download and terminate
    let _ = match handle {
        Ok(h) => h.wait_until_completed().await?,
        _ => {},
    };

    Ok(())
}
```
