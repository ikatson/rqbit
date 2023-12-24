use std::time::Duration;

use anyhow::Context;
use librqbit_core::magnet::Magnet;
use librqbit_dht::DhtBuilder;
use tokio_stream::StreamExt;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let magnet = std::env::args()
        .nth(1)
        .expect("first argument should be a magnet link");
    let magnet = Magnet::parse(&magnet).unwrap();
    let info_hash = magnet.as_id20().context("Supplied magnet link didn't contain a BTv1 infohash")?;

    tracing_subscriber::fmt::init();

    let dht = DhtBuilder::new().await.context("error initializing DHT")?;

    let mut stream = dht.get_peers(info_hash, None)?;

    let stats_printer = async {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            info!("DHT stats: {:?}", dht.stats());
        }
        #[allow(unreachable_code)]
        Ok::<_, anyhow::Error>(())
    };

    let routing_table_dumper = async {
        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;
            dht.with_routing_table(|r| {
                let filename = "/tmp/routing-table.json";
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(filename)
                    .unwrap();
                serde_json::to_writer_pretty(&mut f, r).unwrap();
                info!("Dumped DHT routing table to {}", filename);
            });
        }
        #[allow(unreachable_code)]
        Ok::<_, anyhow::Error>(())
    };

    let peer_printer = async {
        while let Some(peer) = stream.next().await {
            info!("peer found: {}", peer)
        }
        Ok(())
    };

    let res = tokio::select! {
        res = stats_printer => res,
        res = peer_printer => res,
        res = routing_table_dumper => res,
    };
    res
}
