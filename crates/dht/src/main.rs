use std::{collections::HashSet, str::FromStr, time::Duration};

use anyhow::Context;
use dht::{Dht, Id20};
use log::info;
use tokio_stream::StreamExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    pretty_env_logger::init();

    let info_hash = Id20::from_str("64a980abe6e448226bb930ba061592e44c3781a1").unwrap();
    let dht = Dht::new().await.context("error initializing DHT")?;
    let mut stream = dht.get_peers(info_hash).await?;
    let mut seen = HashSet::new();

    let stats_printer = async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            info!("DHT stats: {:?}", dht.stats());
        }
        Ok::<_, anyhow::Error>(())
    };

    let peer_printer = async move {
        while let Some(peer) = stream.next().await {
            let peer = peer.context("error reading peer stream")?;
            if seen.insert(peer) {
                log::info!("peer found: {}", peer)
            }
        }
        Ok(())
    };

    let res = tokio::select! {
        res = stats_printer => res,
        res = peer_printer => res,
    };
    res
}
