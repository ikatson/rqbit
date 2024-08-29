use std::time::Duration;

use librqbit_upnp::{discover_once, discover_services, SSDP_SEARCH_ROOT_ST};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (stx, mut srx) = tokio::sync::mpsc::unbounded_channel::<()>();

    let f1 = async move { discover_once(&tx, SSDP_SEARCH_ROOT_ST, Duration::from_secs(10)).await };

    let f2 = async move {
        while let Some(r) = rx.recv().await {
            let stx = stx.clone();
            tokio::spawn(async move {
                match discover_services(r.location.clone()).await {
                    Ok(s) => {
                        println!("{}: {s:#?}", r.location);
                    }
                    Err(e) => {
                        tracing::error!(location=%r.location, "error discovering")
                    }
                }
                drop(stx);
            });
        }
    };

    let f3 = async move { while (srx.recv().await).is_some() {} };

    tokio::join!(f1, f2, f3);
    Ok(())
}
