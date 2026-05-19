use std::{
    net::{Ipv4Addr, SocketAddr},
    path::Path,
    pin::Pin,
    process::Stdio,
    str::FromStr,
    time::Duration,
};

use anyhow::Context;
use axum::{
    Router,
    body::Body,
    extract::State,
    response::{IntoResponse, Response},
    routing::get,
};
use axum_extra::response::FileStream;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, StatusCode};
use librqbit_upnp_serve::{
    UpnpServer, UpnpServerOptions,
    services::content_directory::{
        ContentDirectoryBrowseProvider,
        browse::response::{Container, Item, ItemOrContainer},
    },
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncSeekExt, BufReader},
    process::ChildStdout,
};
use tokio_util::{io::ReaderStream, sync::CancellationToken};
use tower_http::trace::TraceLayer;
use tracing::{Instrument, debug, error_span, warn};

const PREFIX: &str = "/upnp";
const INPUT_DURATION: Duration = Duration::from_secs(596);
const INPUT_FILE_PATH: &str = "/Users/igor/Movies/big_buck_bunny_720p_h264.mov";

struct Provider {}

impl Provider {
    fn item(&self, http_hostname: &str) -> ItemOrContainer {
        ItemOrContainer::Item(Item {
            id: 1,
            parent_id: 0,
            title: "Example".to_string(),
            mime_type: Some(mime_guess::from_ext("mpeg").first().unwrap()),
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

async fn handler_example_ts(State(port): State<u16>, headers: HeaderMap) -> Response {
    // parse seek headers and other DLNA headers if needed, emit dlna headers necessary
    // just passthrough as mpegts
    // ~/Movies/big_buck_bunny_720p_h264.mov
    tracing::warn!(?headers, "headers");

    let mut status = StatusCode::OK;

    let mut output_headers = HeaderMap::new();

    // This is necessary by the spec
    output_headers.insert(
        "transferMode.dlna.org",
        HeaderValue::from_static("Streaming"),
    );

    // This doesn't matter for my samsung at least, video/mpeg works fine too.
    // output_headers.insert("Content-Type", HeaderValue::from_static("video/x-matroska"));
    output_headers.insert("Content-Type", HeaderValue::from_static("video/mpeg"));

    // CRUCIAL: to tell TV we support seeking only by timestamps (01 is byte ranges, 11 is both).
    output_headers.insert(
        "contentFeatures.dlna.org",
        HeaderValue::from_static("DLNA.ORG_OP=10;DLNA.ORG_FLAGS=81700000000000000000000000000000"),
    );

    let mut ffmpeg = tokio::process::Command::new("ffmpeg");
    ffmpeg
        // less verbosity, only errors
        .args(["-hide_banner", "-loglevel", "error"])
        .arg("-i")
        // Reencode the input stream
        // .arg(format!("http://127.0.0.1:{port}/input.mov"));
    // .arg("http://router.lan:3030/torrents/10/stream/0/The.Dark.Knight.Rises.2012.2160p.UHD.BDRemux.DTS-HD.HDR.DoVi.Hybrid.P8.by.DVT.mkv")
        .arg("http://router.lan:3030/torrents/12/stream/0/Fackham.Hall.2025.iNTERNAL.BluRay.1080p.REMUX.AVC.Dub.DDP.5.1-p3rr3nt.mkv");
    // .arg(INPUT_FILE_PATH);

    // Parse npt seek header. We are only intersted in the first part (start).
    // Assumes XXX.YYY format, not HH:MM:SS.YYY
    // todo: proper npt parsing
    if let Some(npt) = headers.get("timeseekrange.dlna.org") {
        let (start, end) = npt
            .to_str()
            .unwrap()
            .strip_prefix("npt=")
            .unwrap()
            .split_once('-')
            .unwrap();
        tracing::warn!(start, end, "npt");

        // Actually seek.
        ffmpeg
            .arg("-ss")
            .arg(start)
            // CRUCIAL for this to work on Samsung. Otherwise every time you seek
            // UI resets to zero, but ffmpeg keeps playing the seeked stream.
            .arg("-output_ts_offset")
            .arg(start);
        let total = INPUT_DURATION.as_secs();
        output_headers.insert(
            "TimeSeekRange.dlna.org",
            HeaderValue::from_str(&format!("npt={start}-{total}/{total}")).unwrap(),
        );
        tracing::warn!(?output_headers, "output headers");

        // This is required by the spec
        status = StatusCode::PARTIAL_CONTENT;
    }

    ffmpeg.args(["-map", "0", "-c:v", "copy", "-c:a", "copy", "-c:s", "copy"]);

    // e.g. dts, truhd
    // TODO: compute from ffprobe and cache
    let unsupported_audio_streams = [2];
    for id in unsupported_audio_streams {
        ffmpeg.arg(dbg!(format!("-c:{id}"))).arg("ac3");
    }

    let mut ffmpeg = ffmpeg
        .args(["-f", "mpegts", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        // TODO: no panics
        .expect("ffmpeg not found");

    tracing::warn!(?ffmpeg, "running ffmpeg");

    let stdout = ffmpeg.stdout.take().unwrap();
    let stderr = ffmpeg.stderr.take().unwrap();

    let token = CancellationToken::new();

    struct StdoutDropGuard {
        stdout: ChildStdout,
        token: CancellationToken,
    }

    impl Drop for StdoutDropGuard {
        fn drop(&mut self) {
            self.token.cancel();
        }
    }

    impl AsyncRead for StdoutDropGuard {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            Pin::new(&mut self.get_mut().stdout).poll_read(cx, buf)
        }
    }

    let stdout = StdoutDropGuard {
        stdout,
        token: token.clone(),
    };

    // TODO: don't spam logs, this is all for debugging only.
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

    // TODO: add a drop guard on the stream - just before it's dropped, kill ffmpeg, stop
    // reading from it.
    tokio::spawn(
        async move {
            let wait = async move {
                tokio::select! {
                    w = ffmpeg.wait() => w,
                    _ = token.cancelled() => {
                        tracing::warn!("killing ffmpeg");
                        ffmpeg.start_kill()?;
                        ffmpeg.wait().await
                    }
                }
            };
            let (_, wait) = tokio::join!(stderr, wait);
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
    let stdout = nulls::MpegTsNullStuffer::new(stdout, Duration::from_millis(100));
    (status, output_headers, Body::from_stream(stdout)).into_response()
}

impl ContentDirectoryBrowseProvider for Provider {
    fn browse_direct_children(
        &self,
        parent_id: usize,
        http_hostname: &str,
    ) -> Vec<ItemOrContainer> {
        tracing::warn!(parent_id, "browse direct children");
        vec![self.item(http_hostname)]
    }

    fn browse_metadata(&self, object_id: usize, http_hostname: &str) -> Vec<ItemOrContainer> {
        tracing::warn!(object_id, "browse metadata");
        vec![ItemOrContainer::Container(Container {
            id: 0,
            parent_id: None,
            children_count: Some(1),
            title: "root".to_owned(),
        })]
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let mut args = std::env::args();
    args.next().unwrap();

    let name = args.next().context("first arg should be name")?;
    let port: u16 = args
        .next()
        .context("second should be port")?
        .parse()
        .context("second arg port invalid u16")?;

    let token = CancellationToken::new();

    let listener = tokio::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).await?;
    let mut server = UpnpServer::new(UpnpServerOptions {
        friendly_name: name,
        http_listen_port: port,
        http_prefix: PREFIX.to_string(),
        browse_provider: Box::new(Provider {}),
        cancellation_token: token.clone(),
    })
    .await?;
    let router: Router = Router::new()
        .route("/input.mov", get(handler_serve_byte_seek))
        .route("/example.ts", get(handler_example_ts))
        .with_state(port)
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

    let f1 = async {
        tokio::select! {
            r = f1 => r,
            _ = token.cancelled() => {
                tracing::warn!("shutting down axum - cancelled");
                Ok(())
            }
        }
    };

    let f2 = server.run_ssdp_forever();

    let f3 = async {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                std::process::abort();
                // tracing::warn!("ctrl-c received, canceling");
                // token.cancel();
            }
            Err(e) => {
                tracing::warn!("ctrl-c error: {e:#}");
            }
        };
        Ok::<_, anyhow::Error>(())
    };

    tokio::try_join!(f1, f2, f3)?;
    Ok(())
}

mod nulls {
    use std::{
        io,
        pin::Pin,
        task::{Context, Poll},
        time::Duration,
    };

    use bytes::Bytes;
    use futures::Stream;
    use tokio::time::{Sleep, sleep};

    const NULL_MPEGTS_PACKET_RAW: [u8; 188] = {
        let mut p = [0xFFu8; 188];
        p[0] = 0x47;
        p[1] = 0x1F;
        p[2] = 0xFF;
        p[3] = 0x10;
        p
    };
    static NULL_MPEGTS_PACKET: Bytes = Bytes::from_static(&NULL_MPEGTS_PACKET_RAW);

    pub struct MpegTsNullStuffer<S> {
        inner: Pin<Box<S>>,
        deadline: Pin<Box<Sleep>>,
        timeout: Duration,
    }

    impl<S> MpegTsNullStuffer<S>
    where
        S: Stream<Item = io::Result<Bytes>>,
    {
        pub fn new(inner: S, timeout: Duration) -> Self {
            Self {
                deadline: Box::pin(sleep(timeout)),
                inner: Box::pin(inner),
                timeout,
            }
        }
    }

    impl<S> Stream for MpegTsNullStuffer<S>
    where
        S: Stream<Item = io::Result<Bytes>>,
    {
        type Item = io::Result<Bytes>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let timeout = self.timeout;

            // Real data takes priority
            if let Poll::Ready(item) = self.inner.as_mut().poll_next(cx) {
                // Reset deadline on any data
                self.deadline
                    .as_mut()
                    .reset(tokio::time::Instant::now() + timeout);
                return Poll::Ready(item);
            }

            // No data yet — check if deadline has fired
            if self.deadline.as_mut().poll(cx).is_ready() {
                self.deadline
                    .as_mut()
                    .reset(tokio::time::Instant::now() + timeout);
                tracing::trace!("stuffing nulls!");
                return Poll::Ready(Some(Ok(NULL_MPEGTS_PACKET.clone())));
            }

            Poll::Pending
        }
    }
}
