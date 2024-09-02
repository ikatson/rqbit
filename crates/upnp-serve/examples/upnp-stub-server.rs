use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    str::FromStr,
};

use anyhow::Context;
use axum::routing::get;
use librqbit_upnp_serve::{
    services::content_directory::{
        browse::response::{Item, ItemOrContainer},
        ContentDirectoryBrowseProvider,
    },
    UpnpServer, UpnpServerOptions,
};
use mime_guess::Mime;
use tracing::{error, info};

struct VecWrap(Vec<ItemOrContainer>);

impl ContentDirectoryBrowseProvider for VecWrap {
    fn browse_direct_children(&self, _parent_id: usize, _http_host: &str) -> Vec<ItemOrContainer> {
        self.0.clone()
    }

    fn browse_metadata(&self, _object_id: usize, _http_hostname: &str) -> Vec<ItemOrContainer> {
        // TODO. Remove the vec provider from core code.
        vec![]
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "trace");
    }

    tracing_subscriber::fmt::init();

    let items = VecWrap(vec![ItemOrContainer::Item(Item {
        title: "Example".to_owned(),
        mime_type: Some(Mime::from_str("video/x-matroska")?),
        url: "http://192.168.0.165:3030/torrents/4/stream/0/file.mkv".to_owned(),
        id: 1,
        parent_id: 0,
        size: 1,
    })]);

    const HTTP_PORT: u16 = 9005;
    const HTTP_PREFIX: &str = "/upnp";

    info!("Creating UpnpServer");
    let mut server = UpnpServer::new(UpnpServerOptions {
        friendly_name: "demo upnp server".to_owned(),
        http_listen_port: HTTP_PORT,
        http_prefix: HTTP_PREFIX.to_owned(),
        browse_provider: Box::new(items),
        cancellation_token: Default::default(),
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
