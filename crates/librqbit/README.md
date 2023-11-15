# librqbit

A torrent library 100% written in rust

## Basic example
This is a simple program on how to use this library  
This program will just download a simple torrent file with a Magnet link

```rust
use std::error::Error;
use librqbit::session::{AddTorrentResponse, Session};

const MAGNET_LINK: &str = "magnet:?..."; // Put your magnet link here

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>>{

    // Create the session
    let session = Session::new("C:\\Anime".parse().unwrap(), BlockingSpawner::new(false)).await?;

    // Add the torrent to the session
    let handle = match session.add_torrent(MAGNET_LINK, None).await {
        Ok(AddTorrentResponse::Added(handle)) => {
            Ok(handle)
        },
        Err(e) => {
            eprintln!("Something goes wrong when downloading torrent : {:?}", e);
            Err(())
        }
        _ => Err(())
    }.expect("Failed to add torrent to the session");

    // Wait until the download is completed
    handle.wait_until_completed().await?;

    Ok(())
}
```
