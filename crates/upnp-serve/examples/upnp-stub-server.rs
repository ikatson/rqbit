use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use anyhow::Context;
use axum::routing::get;
use tracing::{error, info};
use upnp_serve::{ContentDirectoryBrowseItem, UpnpServer, UpnpServerOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "trace");
    }

    tracing_subscriber::fmt::init();

    let items: Vec<ContentDirectoryBrowseItem> = vec![ContentDirectoryBrowseItem {
        title: "Example".to_owned(),
        mime_type: Some("video/x-matroska".to_owned()),
        url: "http://192.168.0.165:3030/torrents/4/stream/0".to_owned(),
    }];

    const HTTP_PORT: u16 = 9005;
    const HTTP_PREFIX: &str = "/upnp";

    info!("Creating UpnpServer");
    let mut server = UpnpServer::new(UpnpServerOptions {
        friendly_name: "demo upnp server".to_owned(),
        http_hostname: std::env::var("UPNP_HOSTNAME")
            .context("you need to set UPNP_HOSTNAME to your IP visible from LAN")?,
        http_listen_port: HTTP_PORT,
        http_prefix: HTTP_PREFIX.to_owned(),
        browse_provider: Box::new(items),
    })
    .await?;

    let app = axum::Router::new()
        .route("/", get(|| async { "hello world" }))
        .nest(HTTP_PREFIX, server.take_router()?)
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .into_make_service_with_connect_info::<SocketAddr>();

    use tokio::net::TcpListener;

    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, HTTP_PORT);

    info!(?addr, "Binding TcpListener");
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("error binding to {addr}"))?;

    tokio::spawn(async move {
        let res = axum::serve(listener, app).await;
        error!(error=?res, "error running HTTP server");
    });

    info!("Running SSDP");
    server
        .run_ssdp_forever()
        .await
        .context("error running SSDP")?;

    error!("Unreachable");
    Ok(())
}
