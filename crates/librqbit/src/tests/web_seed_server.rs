//! An HTTP/1.1 server on loopback that serves files the way a web seed (BEP 19) does, and can be
//! told to misbehave: ignore `Range`, answer with the wrong length, corrupt bytes, answer 503,
//! not have a file, never answer, or send the body slowly. It records every request, and sees
//! the client close a connection while an answer was pending.

use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr},
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, bail};
use bytes::{Buf, BytesMut};
use parking_lot::Mutex;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        TcpListener, TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    task::JoinSet,
};

/// How the server answers. Can be changed while it runs.
#[derive(Default, Clone, Debug)]
pub struct Behaviour {
    /// Answer every request with `200` and the whole file.
    pub ignore_range: bool,
    /// Answer range requests with one byte less than asked for.
    pub wrong_length: bool,
    /// Flip the bytes of these files in these ranges (file offsets).
    pub corrupt: Vec<(String, Range<u64>)>,
    /// Answer this many of the next requests with `503` and `Retry-After: 1`.
    pub unavailable: u32,
    /// Answer `404` for these files.
    pub missing: Vec<String>,
    /// Read requests but never answer them.
    pub stall: bool,
    /// Send bodies `.0` bytes at a time, waiting `.1` between them.
    pub drip: Option<(usize, Duration)>,
}

/// A request the server got.
#[derive(Clone, Debug)]
pub struct Served {
    /// The percent-decoded path.
    pub path: String,
    /// The `Range` header, inclusive.
    pub range: Option<(u64, u64)>,
    /// The status of the answer; 0 when none was sent.
    pub status: u16,
}

struct State {
    files: HashMap<String, Vec<u8>>,
    behaviour: Mutex<Behaviour>,
    log: Mutex<Vec<Served>>,
    in_flight: AtomicUsize,
    aborted: AtomicUsize,
    body_bytes: AtomicU64,
}

pub struct WebSeedServer {
    pub addr: SocketAddr,
    state: Arc<State>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for WebSeedServer {
    fn drop(&mut self) {
        // Aborting the accept loop drops its JoinSet, which aborts every connection.
        self.task.abort();
    }
}

impl WebSeedServer {
    /// Serves `files`, keyed by their percent-decoded URL path (`/seed/name/dir/file`).
    pub async fn start(files: impl IntoIterator<Item = (String, Vec<u8>)>) -> anyhow::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = listener.local_addr()?;
        let state = Arc::new(State {
            files: files.into_iter().collect(),
            behaviour: Default::default(),
            log: Default::default(),
            in_flight: AtomicUsize::new(0),
            aborted: AtomicUsize::new(0),
            body_bytes: AtomicU64::new(0),
        });
        let task = tokio::spawn(accept(listener, state.clone()));
        Ok(Self { addr, state, task })
    }

    /// The base URL of the web seed: `http://127.0.0.1:port/seed/`.
    pub fn url(&self) -> String {
        format!("http://{}/seed/", self.addr)
    }

    pub fn set(&self, f: impl FnOnce(&mut Behaviour)) {
        f(&mut self.state.behaviour.lock());
    }

    pub fn log(&self) -> Vec<Served> {
        self.state.log.lock().clone()
    }

    /// Requests that got an answer with this status, for `path`.
    pub fn served(&self, path: &str, status: u16) -> usize {
        self.state
            .log
            .lock()
            .iter()
            .filter(|s| s.path == path && s.status == status)
            .count()
    }

    /// Requests whose answer is not complete yet.
    pub fn in_flight(&self) -> usize {
        self.state.in_flight.load(Ordering::SeqCst)
    }

    /// Requests the client gave up on: it closed the connection before the answer was complete.
    pub fn aborted(&self) -> usize {
        self.state.aborted.load(Ordering::SeqCst)
    }

    /// Body bytes written.
    pub fn body_bytes(&self) -> u64 {
        self.state.body_bytes.load(Ordering::SeqCst)
    }
}

async fn accept(listener: TcpListener, state: Arc<State>) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            r = listener.accept() => match r {
                Ok((stream, _)) => {
                    connections.spawn(serve(stream, state.clone()));
                }
                Err(e) => {
                    tracing::warn!("web seed server: accept failed: {e:#}");
                    return;
                }
            },
            Some(_) = connections.join_next() => {}
        }
    }
}

/// Counts a request as in flight until the guard is dropped; dropped early, it counts as
/// aborted.
struct InFlight<'a> {
    state: &'a State,
    done: bool,
}

impl<'a> InFlight<'a> {
    fn new(state: &'a State) -> Self {
        state.in_flight.fetch_add(1, Ordering::SeqCst);
        Self { state, done: false }
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.state.aborted.fetch_add(1, Ordering::SeqCst);
        }
        self.state.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Request {
    path: String,
    range: Option<(u64, u64)>,
}

async fn serve(stream: TcpStream, state: Arc<State>) {
    let (mut reader, mut writer) = stream.into_split();
    // What was read past the request being answered (pipelined requests).
    let mut pending = BytesMut::new();
    loop {
        let request = match read_request(&mut reader, &mut pending).await {
            Ok(Some(r)) => r,
            Ok(None) => return,
            Err(e) => {
                tracing::debug!("web seed server: bad request: {e:#}");
                return;
            }
        };
        let mut guard = InFlight::new(&state);
        let behaviour = state.behaviour.lock().clone();
        let log_index = {
            let mut log = state.log.lock();
            log.push(Served {
                path: request.path.clone(),
                range: request.range,
                status: 0,
            });
            log.len() - 1
        };
        if behaviour.stall {
            // Until the client goes away.
            let _ = wait_closed(&mut reader, &mut pending).await;
            return;
        }
        let (status, headers, body) = answer(&state, &request);
        state.log.lock()[log_index].status = status;
        let mut head = format!(
            "HTTP/1.1 {status} {}\r\nContent-Length: {}\r\n",
            reason(status),
            body.len()
        );
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        if writer.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        if send_body(
            &state,
            &mut reader,
            &mut writer,
            &mut pending,
            &body,
            behaviour.drip,
        )
        .await
        .is_err()
        {
            return;
        }
        guard.done = true;
    }
}

/// The answer to `request`: status, extra headers and body.
fn answer(state: &State, request: &Request) -> (u16, Vec<(&'static str, String)>, Vec<u8>) {
    let mut behaviour = state.behaviour.lock();
    if behaviour.unavailable > 0 {
        behaviour.unavailable -= 1;
        return (503, vec![("Retry-After", "1".to_owned())], Vec::new());
    }
    if behaviour.missing.contains(&request.path) {
        return (404, Vec::new(), Vec::new());
    }
    let Some(content) = state.files.get(&request.path) else {
        return (404, Vec::new(), Vec::new());
    };
    let mut content = content.clone();
    // An offset into `content`, clamped to its length.
    let at = |v: u64, len: usize| usize::try_from(v).map_or(len, |v| v.min(len));
    for (path, range) in &behaviour.corrupt {
        if *path == request.path {
            let end = at(range.end, content.len());
            for b in &mut content[at(range.start, end)..end] {
                *b ^= 0xff;
            }
        }
    }
    let len = content.len() as u64;
    match request.range {
        Some((first, last)) if !behaviour.ignore_range => {
            if first > last || last >= len {
                return (
                    416,
                    vec![("Content-Range", format!("bytes */{len}"))],
                    Vec::new(),
                );
            }
            let mut end = last + 1;
            if behaviour.wrong_length {
                end -= 1;
            }
            let content_range = format!("bytes {first}-{}/{len}", end.max(first + 1) - 1);
            (
                206,
                vec![("Content-Range", content_range)],
                content[at(first, content.len())..at(end, content.len())].to_vec(),
            )
        }
        _ => (200, Vec::new(), content),
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        206 => "Partial Content",
        404 => "Not Found",
        416 => "Range Not Satisfiable",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

async fn send_body(
    state: &State,
    reader: &mut OwnedReadHalf,
    writer: &mut OwnedWriteHalf,
    pending: &mut BytesMut,
    body: &[u8],
    drip: Option<(usize, Duration)>,
) -> anyhow::Result<()> {
    let step = drip.map_or(64 * 1024, |(n, _)| n.max(1));
    for (i, part) in body.chunks(step).enumerate() {
        if let Some((_, delay)) = drip
            && i > 0
        {
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                r = wait_closed(reader, pending) => {
                    r?;
                    bail!("closed");
                }
            }
        }
        tokio::select! {
            r = writer.write_all(part) => r?,
            r = wait_closed(reader, pending) => {
                r?;
                bail!("closed");
            }
        }
        state
            .body_bytes
            .fetch_add(part.len() as u64, Ordering::SeqCst);
    }
    writer.flush().await?;
    Ok(())
}

/// Reads (keeping what arrives for the next request) until the client closes the connection.
async fn wait_closed(reader: &mut OwnedReadHalf, pending: &mut BytesMut) -> anyhow::Result<()> {
    loop {
        if pending.len() > 64 * 1024 {
            bail!("too much pipelined");
        }
        if reader.read_buf(pending).await? == 0 {
            return Ok(());
        }
    }
}

/// Reads the next request; `None` when the client closed the connection between requests.
async fn read_request(
    reader: &mut OwnedReadHalf,
    pending: &mut BytesMut,
) -> anyhow::Result<Option<Request>> {
    let head_len = loop {
        if let Some(pos) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        if pending.len() > 16 * 1024 {
            bail!("request head too long");
        }
        if reader.read_buf(pending).await? == 0 {
            if pending.is_empty() {
                return Ok(None);
            }
            bail!("connection closed inside a request");
        }
    };
    let head = String::from_utf8(pending[..head_len].to_vec()).context("request not UTF-8")?;
    pending.advance(head_len);
    let mut lines = head.split("\r\n");
    let request_line = lines.next().context("no request line")?;
    let mut parts = request_line.split(' ');
    let method = parts.next().context("no method")?;
    if method != "GET" {
        bail!("unsupported method {method}");
    }
    let target = parts.next().context("no target")?;
    let path = target.split('?').next().unwrap_or_default();
    let path = String::from_utf8(percent_decode(path)?).context("path not UTF-8")?;
    let mut range = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("range") {
            range = Some(parse_range(value.trim()).context("bad Range")?);
        }
    }
    Ok(Some(Request { path, range }))
}

fn parse_range(value: &str) -> Option<(u64, u64)> {
    let (first, last) = value.strip_prefix("bytes=")?.split_once('-')?;
    Some((first.trim().parse().ok()?, last.trim().parse().ok()?))
}

fn percent_decode(s: &str) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hex = [
                bytes.next().context("short escape")?,
                bytes.next().context("short escape")?,
            ];
            let hex = std::str::from_utf8(&hex)?;
            out.push(u8::from_str_radix(hex, 16)?);
        } else {
            out.push(b);
        }
    }
    Ok(out)
}
