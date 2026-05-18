use std::{
    net::{Ipv4Addr, SocketAddr},
    path::Path,
    process::Stdio,
};

use axum::{
    Router,
    body::Body,
    response::{IntoResponse, Response},
    routing::get,
};
use axum_extra::response::FileStream;
use http::{HeaderMap, HeaderValue, StatusCode};
use librqbit_upnp_serve::{
    UpnpServer, UpnpServerOptions,
    services::content_directory::{
        ContentDirectoryBrowseProvider,
        browse::response::{Item, ItemOrContainer},
    },
};
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};
use tokio_util::{io::ReaderStream, sync::CancellationToken};
use tower_http::trace::TraceLayer;
use tracing::{Instrument, debug, error_span};

const PORT: u16 = 6819;
const PREFIX: &str = "/upnp";
const INPUT_FILE_PATH: &str = "/Users/igor/Movies/big_buck_bunny_720p_h264.mov";

struct Provider {}

impl Provider {
    fn item(&self, http_hostname: &str) -> ItemOrContainer {
        ItemOrContainer::Item(Item {
            id: 1,
            parent_id: 0,
            title: "Example".to_string(),
            mime_type: Some(mime_guess::from_ext("ts").first().unwrap()),
            url: format!("http://{http_hostname}/example.ts"),
            size: 0,
        })
    }
}

async fn handler_serve_byte_seek(headers: HeaderMap) -> Response {
    let mut output_headers = HeaderMap::new();
    output_headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));
    let range_header = headers.get(http::header::RANGE);

    let range = range_header
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| v.split_once('-'))
        .and_then(|(start, end)| {
            let start = start.parse::<u64>().ok()?;
            let end = if end.is_empty() {
                None
            } else {
                Some(end.parse::<u64>().ok()?.saturating_add(1))
            };
            Some((start, end))
        });

    tracing::info!(?range, "request with range");

    let mut file = tokio::fs::File::open(INPUT_FILE_PATH).await.unwrap();
    let size = file.metadata().await.unwrap().len();

    if let Some((start, end)) = range {
        file.seek(std::io::SeekFrom::Start(start)).await.unwrap();
        let stream = ReaderStream::new(file);
        let end = end.unwrap_or(size - 1);
        FileStream::new(stream).into_range_response(start, end, size)
    } else {
        let stream = ReaderStream::new(file);
        FileStream::new(stream).into_response()
    }
}

async fn handler_example_ts(headers: HeaderMap) -> Response {
    // parse seek headers and other DLNA headers if needed, emit dlna headers necessary
    // just passthrough as mpegts
    // ~/Movies/big_buck_bunny_720p_h264.mov
    let mut ffmpeg = tokio::process::Command::new("ffmpeg")
        .arg("-i")
        .arg(format!("http://127.0.0.1:{PORT}/input.mov"))
        .args(["-hide_banner", "-loglevel", "error"])
        .args([
            "-vcodec", "copy", "-acodec", "copy", "-f", "mpegts", "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("ffmpeg not found");

    let stdout = ffmpeg.stdout.take().unwrap();
    let stderr = ffmpeg.stderr.take().unwrap();

    let stderr = async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Some(line) = lines.next_line().await.transpose() {
            match line {
                Ok(line) => tracing::warn!(line),
                Err(e) => {
                    tracing::error!("{e:#}")
                }
            }
        }
    };

    tokio::spawn(
        async move {
            let (_, wait) = tokio::join!(stderr, ffmpeg.wait());
            match wait {
                Ok(wait) => {
                    if wait.success() {
                        tracing::info!("success")
                    } else {
                        tracing::warn!(?wait, "ffmpeg exited")
                    }
                }
                Err(e) => {
                    tracing::error!("error waiting: {e:#}")
                }
            }
        }
        .instrument(error_span!("ffmpeg")),
    );

    let stdout = tokio_util::io::ReaderStream::new(stdout);
    Body::from_stream(stdout).into_response()
}

impl ContentDirectoryBrowseProvider for Provider {
    fn browse_direct_children(
        &self,
        parent_id: usize,
        http_hostname: &str,
    ) -> Vec<ItemOrContainer> {
        vec![self.item(http_hostname)]
    }

    fn browse_metadata(&self, object_id: usize, http_hostname: &str) -> Vec<ItemOrContainer> {
        vec![]
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let listener = tokio::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, PORT)).await?;
    let mut server = UpnpServer::new(UpnpServerOptions {
        friendly_name: "transcode-test".to_string(),
        http_listen_port: PORT,
        http_prefix: PREFIX.to_string(),
        browse_provider: Box::new(Provider {}),
        cancellation_token: CancellationToken::new(),
    })
    .await?;
    let router: Router = Router::new()
        .route("/input.mov", get(handler_serve_byte_seek))
        .route("/example.ts", get(handler_example_ts))
        .nest(PREFIX, server.take_router().unwrap())
        .layer(TraceLayer::new_for_http());

    let f1 = async {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await?;
        Ok(())
    };

    let f2 = server.run_ssdp_forever();
    tokio::try_join!(f1, f2)?;
    Ok(())
}
