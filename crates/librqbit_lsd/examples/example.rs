use std::{str::FromStr, time::Duration};

use atoi::atoi;
use futures::StreamExt;
use librqbit_core::Id20;
use librqbit_lsd::LocalServiceDiscovery;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match std::env::var("RUST_LOG") {
        Ok(_) => {}
        Err(_) => unsafe { std::env::set_var("RUST_LOG", "info") },
    }
    tracing_subscriber::fmt::init();

    let lsd = LocalServiceDiscovery::new(Default::default())?;
    let args = std::env::args().collect::<Vec<_>>();

    match (args.get(1), args.get(2)) {
        (Some(h), Some(p)) => {
            let info_hash = Id20::from_str(h).unwrap();
            let port = atoi(p.as_bytes());
            let mut stream = lsd.announce(info_hash, port);
            while let Some(addr) = stream.next().await {
                info!(?addr, "got addr from LSD")
            }
        }
        _ => loop {
            tokio::time::sleep(Duration::from_secs(60)).await
        },
    }

    Ok(())
}
