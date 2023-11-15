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

    let session = Session::new("C:\\Anime", Default::default());
    let handle = match session.add_torrent(MAGNET_LINK, None).await {
        AddTorrentResponse::Added(handle) => {
            handle
        },
        resp => unimplemented!("{:?}", resp)
    };
    handle.wait_until_completed().await?;

    Ok(())
}
```
