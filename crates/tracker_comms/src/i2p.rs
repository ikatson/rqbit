//! Trackers inside the I2P network ("BitTorrent over I2P",
//! <https://geti2p.net/en/docs/applications/bittorrent>).
//!
//! An I2P tracker is an HTTP tracker whose host ends in `.i2p`. It is reached through a
//! stream that the I2P router opens to it (see [`I2pTransport`]), and HTTP/1.1 is spoken over
//! that stream by the minimal client of this module. The announce tells the tracker our own
//! destination (`ip=<base64 destination>.i2p`) instead of an address, and the peers it returns
//! are destinations: 32-byte hashes in the compact form, base64 destinations in the dictionary
//! form. The transport turns them into the socket addresses the rest of the client uses.

use std::{net::SocketAddr, sync::Arc};

use anyhow::{Context, bail};
use buffers::ByteBuf;
use futures::future::BoxFuture;
use librqbit_core::i2p::{B32_SUFFIX, I2pDestination, I2pHash};
use serde::Deserializer;
use serde_derive::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// A byte stream to a host inside I2P.
pub trait I2pStreamIo: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> I2pStreamIo for T {}

/// A stream to an I2P tracker, opened by [`I2pTransport::connect`].
pub struct I2pTrackerStream {
    pub stream: Box<dyn I2pStreamIo>,
    /// Our own destination in the I2P base64 form, announced as `ip=<destination>.i2p`.
    pub our_destination: String,
}

/// What the trackers need from the I2P router.
pub trait I2pTransport: Send + Sync {
    /// Opens a stream to `host` (`name.i2p` or `….b32.i2p`).
    fn connect(&self, host: String) -> BoxFuture<'static, anyhow::Result<I2pTrackerStream>>;
    /// The socket address under which the peer with this destination hash is known.
    fn peer_from_hash(&self, hash: I2pHash) -> SocketAddr;
    /// The socket address under which the peer with this destination is known.
    fn peer_from_destination(&self, destination: I2pDestination) -> SocketAddr;
    /// The `User-Agent` of the requests.
    fn user_agent(&self) -> &str;
}

/// Whether and how the trackers of a torrent use I2P.
#[derive(Clone)]
pub enum I2pTrackers {
    /// The `.i2p` trackers are not used; the text says why and is their error.
    Off(&'static str),
    /// The `.i2p` trackers are reached through `transport`. The other trackers are used only
    /// with `clearnet`; without it they are skipped with an error saying so.
    On {
        transport: Arc<dyn I2pTransport>,
        clearnet: bool,
    },
}

/// The error of the `.i2p` trackers when the session has no I2P.
pub const I2P_NOT_ENABLED: &str = "an I2P tracker, but I2P is not enabled in the session";

/// The error of the clearnet trackers of an I2P torrent when mixed mode is off: they are
/// skipped, the torrent stays inside I2P.
pub const I2P_CLEARNET_SKIPPED: &str =
    "a clearnet tracker of an I2P torrent, skipped: I2P mixed mode is off";

impl Default for I2pTrackers {
    fn default() -> Self {
        Self::Off(I2P_NOT_ENABLED)
    }
}

impl std::fmt::Debug for I2pTrackers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off(why) => f.debug_tuple("Off").field(why).finish(),
            Self::On { clearnet, .. } => f.debug_struct("On").field("clearnet", clearnet).finish(),
        }
    }
}

/// The port announced to I2P trackers. I2P has no ports, but trackers want one.
pub(crate) const I2P_ANNOUNCE_PORT: u16 = 6881;

/// At most this many peers are taken from one response: every one of them takes a place in
/// the address book of the session, which a hostile tracker must not be able to flush.
pub(crate) const MAX_PEERS_PER_RESPONSE: usize = 500;

/// Longest accepted response head (status line and headers).
const MAX_HEAD_LEN: usize = 16 * 1024;
/// Longest accepted line inside a chunked body (chunk sizes, trailers).
const MAX_CHUNK_LINE_LEN: u64 = 1024;
/// Most headers in a response head.
const MAX_HEADERS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyLength {
    Fixed(u64),
    Chunked,
    UntilEof,
}

/// Sends `GET <target>` to `host` over `stream` (`Connection: close`) and returns the body
/// of a 2xx answer, at most `max_body` bytes of it.
pub(crate) async fn http_get(
    stream: Box<dyn I2pStreamIo>,
    host: &str,
    target: &str,
    user_agent: &str,
    max_body: usize,
) -> anyhow::Result<Vec<u8>> {
    for (what, value) in [
        ("host", host),
        ("target", target),
        ("user agent", user_agent),
    ] {
        if value.bytes().any(|b| b.is_ascii_control()) {
            bail!("the request {what} contains a control character");
        }
    }
    let mut stream = BufReader::new(stream);
    let request = format!(
        "GET {target} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: {user_agent}\r\n\
         Accept-Encoding: identity\r\nConnection: close\r\n\r\n"
    );
    stream.get_mut().write_all(request.as_bytes()).await?;
    stream.get_mut().flush().await?;

    let head = read_head(&mut stream).await?;
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut response = httparse::Response::new(&mut headers);
    match response
        .parse(&head)
        .context("malformed HTTP response head")?
    {
        httparse::Status::Complete(_) => {}
        httparse::Status::Partial => bail!("incomplete HTTP response head"),
    }
    let code = response.code.context("HTTP response without a status")?;
    if !(200..300).contains(&code) {
        bail!("tracker responded with HTTP {code}");
    }
    let length = body_length(response.headers)?;

    let max = max_body as u64;
    let mut body = Vec::new();
    match length {
        BodyLength::Fixed(len) => {
            if len > max {
                bail!("tracker response is bigger than {max_body} bytes");
            }
            let got = (&mut stream).take(len).read_to_end(&mut body).await?;
            if got as u64 != len {
                bail!("tracker response ended after {got} of {len} bytes");
            }
        }
        BodyLength::UntilEof => {
            (&mut stream).take(max + 1).read_to_end(&mut body).await?;
            if body.len() > max_body {
                bail!("tracker response is bigger than {max_body} bytes");
            }
        }
        BodyLength::Chunked => loop {
            let line = read_line(&mut stream, MAX_CHUNK_LINE_LEN).await?;
            let size = line.split(';').next().unwrap_or_default().trim();
            let size = u64::from_str_radix(size, 16)
                .with_context(|| format!("bad chunk size {size:?}"))?;
            if size == 0 {
                // Trailers, up to the empty line.
                let mut trailers = 0;
                while !read_line(&mut stream, MAX_CHUNK_LINE_LEN).await?.is_empty() {
                    trailers += 1;
                    if trailers > MAX_HEADERS {
                        bail!("too many trailers");
                    }
                }
                break;
            }
            if body.len() as u64 + size > max {
                bail!("tracker response is bigger than {max_body} bytes");
            }
            let before = body.len();
            (&mut stream).take(size).read_to_end(&mut body).await?;
            if (body.len() - before) as u64 != size {
                bail!("a chunk of the tracker response was cut short");
            }
            if !read_line(&mut stream, MAX_CHUNK_LINE_LEN).await?.is_empty() {
                bail!("a chunk is not followed by a line break");
            }
        },
    }
    Ok(body)
}

/// Reads the response head, up to and including the empty line.
async fn read_head(stream: &mut BufReader<Box<dyn I2pStreamIo>>) -> anyhow::Result<Vec<u8>> {
    let mut head = Vec::new();
    loop {
        let start = head.len();
        let left = (MAX_HEAD_LEN - start) as u64;
        let n = (&mut *stream)
            .take(left)
            .read_until(b'\n', &mut head)
            .await?;
        if n == 0 {
            if head.len() >= MAX_HEAD_LEN {
                bail!("HTTP response head is longer than {MAX_HEAD_LEN} bytes");
            }
            bail!("the tracker closed the stream before the response head ended");
        }
        if !head.ends_with(b"\n") {
            if head.len() >= MAX_HEAD_LEN {
                bail!("HTTP response head is longer than {MAX_HEAD_LEN} bytes");
            }
            bail!("the tracker closed the stream before the response head ended");
        }
        let line = &head[start..];
        if line == b"\r\n" || line == b"\n" {
            return Ok(head);
        }
    }
}

/// Reads one line of at most `max` bytes, without its line break.
async fn read_line(
    stream: &mut BufReader<Box<dyn I2pStreamIo>>,
    max: u64,
) -> anyhow::Result<String> {
    let mut line = Vec::new();
    (&mut *stream)
        .take(max)
        .read_until(b'\n', &mut line)
        .await?;
    if !line.ends_with(b"\n") {
        bail!("a line of the chunked body is cut short or too long");
    }
    line.pop();
    if line.ends_with(b"\r") {
        line.pop();
    }
    String::from_utf8(line).context("a line of the chunked body is not UTF-8")
}

fn body_length(headers: &[httparse::Header<'_>]) -> anyhow::Result<BodyLength> {
    let mut length = None;
    let mut chunked = false;
    for h in headers {
        if h.name.eq_ignore_ascii_case("transfer-encoding") {
            let value = std::str::from_utf8(h.value).context("bad Transfer-Encoding")?;
            for coding in value.split(',').map(str::trim) {
                if coding.eq_ignore_ascii_case("chunked") {
                    chunked = true;
                } else if !coding.eq_ignore_ascii_case("identity") {
                    bail!("unsupported Transfer-Encoding {coding:?}");
                }
            }
        } else if h.name.eq_ignore_ascii_case("content-length") {
            let value: u64 = std::str::from_utf8(h.value)
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .context("bad Content-Length")?;
            if length.is_some_and(|l| l != value) {
                bail!("conflicting Content-Length headers");
            }
            length = Some(value);
        }
    }
    // RFC 9112 6.3: chunked wins over Content-Length.
    Ok(match (chunked, length) {
        (true, _) => BodyLength::Chunked,
        (false, Some(len)) => BodyLength::Fixed(len),
        (false, None) => BodyLength::UntilEof,
    })
}

/// The announce response of an I2P tracker.
#[derive(Deserialize, Debug)]
pub(crate) struct I2pTrackerResponse<'a> {
    #[serde(rename = "warning message", borrow)]
    pub warning_message: Option<ByteBuf<'a>>,
    pub complete: Option<i64>,
    pub interval: Option<u64>,
    #[serde(rename = "min interval")]
    pub min_interval: Option<u64>,
    #[serde(rename = "tracker id", borrow)]
    pub tracker_id: Option<ByteBuf<'a>>,
    pub incomplete: Option<i64>,
    pub downloaded: Option<i64>,
    #[serde(default, borrow)]
    pub peers: I2pPeers<'a>,
}

/// The peers of an I2P announce response.
#[derive(Debug)]
pub(crate) enum I2pPeers<'a> {
    /// Concatenated 32-byte destination hashes.
    Compact(&'a [u8]),
    /// The `ip` of every dictionary: a base64 destination, usually followed by `.i2p`.
    Dict(Vec<ByteBuf<'a>>),
}

impl Default for I2pPeers<'_> {
    fn default() -> Self {
        Self::Dict(Vec::new())
    }
}

impl<'a, 'de: 'a> serde::de::Deserialize<'de> for I2pPeers<'a> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct DictPeer<'a> {
            #[serde(borrow)]
            ip: ByteBuf<'a>,
        }

        struct Visitor<'a>(std::marker::PhantomData<&'a ()>);
        impl<'a, 'de: 'a> serde::de::Visitor<'de> for Visitor<'a> {
            type Value = I2pPeers<'a>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a list of I2P peers in dict or compact format")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut peers = Vec::new();
                while let Some(peer) = seq.next_element::<DictPeer<'de>>()? {
                    // Read on to check the syntax, keep only what is used.
                    if peers.len() < MAX_PEERS_PER_RESPONSE {
                        peers.push(peer.ip);
                    }
                }
                Ok(I2pPeers::Dict(peers))
            }

            fn visit_borrowed_bytes<E>(self, v: &'de [u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(I2pPeers::Compact(v))
            }
        }
        deserializer.deserialize_any(Visitor(std::marker::PhantomData))
    }
}

impl I2pPeers<'_> {
    /// The peers as socket addresses, through `transport`. Entries that are not destinations
    /// are skipped; a compact list whose length is not a multiple of 32 is an error.
    pub(crate) fn addrs(&self, transport: &dyn I2pTransport) -> anyhow::Result<Vec<SocketAddr>> {
        match self {
            I2pPeers::Compact(bytes) => {
                if bytes.len() % 32 != 0 {
                    bail!(
                        "compact I2P peers are 32 bytes each, got a list of {} bytes",
                        bytes.len()
                    );
                }
                Ok(bytes
                    .as_chunks::<32>()
                    .0
                    .iter()
                    .take(MAX_PEERS_PER_RESPONSE)
                    .map(|h| transport.peer_from_hash(I2pHash(*h)))
                    .collect())
            }
            I2pPeers::Dict(ips) => Ok(ips
                .iter()
                .filter_map(|ip| {
                    let peer = parse_dict_peer(ip.as_ref());
                    if peer.is_none() {
                        tracing::debug!(ip = ?ip, "skipping an I2P peer that is not a destination");
                    }
                    peer
                })
                .map(|peer| match peer {
                    DictPeer::Hash(h) => transport.peer_from_hash(h),
                    DictPeer::Destination(d) => transport.peer_from_destination(d),
                })
                .collect()),
        }
    }
}

enum DictPeer {
    Hash(I2pHash),
    Destination(I2pDestination),
}

/// `<base64 destination>[.i2p]` or `….b32.i2p`.
fn parse_dict_peer(ip: &[u8]) -> Option<DictPeer> {
    let ip = std::str::from_utf8(ip).ok()?;
    if ip.len() > B32_SUFFIX.len()
        && ip
            .get(ip.len() - B32_SUFFIX.len()..)
            .is_some_and(|s| s.eq_ignore_ascii_case(B32_SUFFIX))
    {
        return I2pHash::from_b32_name(ip).map(DictPeer::Hash);
    }
    let base64 = match ip.len().checked_sub(4).and_then(|at| ip.get(at..)) {
        Some(suffix) if suffix.eq_ignore_ascii_case(".i2p") => &ip[..ip.len() - 4],
        _ => ip,
    };
    I2pDestination::from_i2p_base64(base64)
        .ok()
        .map(DictPeer::Destination)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{collections::HashMap, net::SocketAddr};

    use librqbit_core::i2p::{
        I2pDestination, I2pHash, i2p_base64_encode, is_synthetic, synthetic_addr,
    };
    use parking_lot::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// A destination whose bytes are all `seed`, with a null certificate.
    pub(crate) fn dest(seed: u8) -> I2pDestination {
        let mut d = vec![seed; 384];
        d.extend_from_slice(&[0, 0, 0]);
        I2pDestination::from_i2p_base64(&i2p_base64_encode(&d)).unwrap()
    }

    /// A transport that reaches every I2P host it knows over plain TCP on loopback, and whose
    /// address book counts up from 1.
    #[derive(Default)]
    pub(crate) struct FakeTransport {
        /// Host name -> where its "I2P stream" really goes.
        pub hosts: Mutex<HashMap<String, SocketAddr>>,
        /// Every host a stream was asked for, in order.
        pub connects: Mutex<Vec<String>>,
        /// While set, every stream fails as if the bridge were down.
        pub down: std::sync::atomic::AtomicBool,
        book: Mutex<HashMap<I2pHash, u64>>,
    }

    impl FakeTransport {
        pub(crate) fn with_host(host: &str, addr: SocketAddr) -> Arc<Self> {
            let t = Self::default();
            t.hosts.lock().insert(host.to_owned(), addr);
            Arc::new(t)
        }

        pub(crate) fn addr_of(&self, hash: I2pHash) -> SocketAddr {
            let mut g = self.book.lock();
            let next = g.len() as u64 + 1;
            synthetic_addr(*g.entry(hash).or_insert(next))
        }
    }

    impl I2pTransport for FakeTransport {
        fn connect(&self, host: String) -> BoxFuture<'static, anyhow::Result<I2pTrackerStream>> {
            self.connects.lock().push(host.clone());
            let target = self.hosts.lock().get(&host).copied();
            let down = self.down.load(std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                if down {
                    bail!("the SAM bridge is down");
                }
                let target = target.with_context(|| format!("no such destination {host}"))?;
                let stream = tokio::net::TcpStream::connect(target).await?;
                Ok(I2pTrackerStream {
                    stream: Box::new(stream),
                    our_destination: dest(9).to_i2p_base64(),
                })
            })
        }

        fn peer_from_hash(&self, hash: I2pHash) -> SocketAddr {
            self.addr_of(hash)
        }

        fn peer_from_destination(&self, destination: I2pDestination) -> SocketAddr {
            self.addr_of(destination.hash())
        }

        fn user_agent(&self) -> &str {
            "test/1.0"
        }
    }

    /// Runs `http_get` against a server that answers any request with `response`, and
    /// returns the result and the request the server got.
    async fn get(response: Vec<u8>, max_body: usize) -> (anyhow::Result<Vec<u8>>, String) {
        let (ours, mut theirs) = tokio::io::duplex(1 << 16);
        let server = tokio::spawn(async move {
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = theirs.read(&mut buf).await.unwrap();
                assert_ne!(n, 0, "the request ended early");
                request.extend_from_slice(&buf[..n]);
            }
            // The client may stop reading early; that is not a failure here.
            let _ = theirs.write_all(&response).await;
            let _ = theirs.shutdown().await;
            String::from_utf8(request).unwrap()
        });
        let res = http_get(
            Box::new(ours),
            "tracker.i2p",
            "/a?x=1",
            "test/1.0",
            max_body,
        )
        .await;
        (res, server.await.unwrap())
    }

    #[tokio::test]
    async fn request_line_and_headers() {
        let (res, request) = get(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi".to_vec(),
            100,
        )
        .await;
        assert_eq!(res.unwrap(), b"hi");
        assert_eq!(
            request,
            "GET /a?x=1 HTTP/1.1\r\nHost: tracker.i2p\r\nUser-Agent: test/1.0\r\n\
             Accept-Encoding: identity\r\nConnection: close\r\n\r\n"
        );
    }

    #[tokio::test]
    async fn bodies_by_length_chunks_and_eof() {
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                        3;ext=1\r\nabc\r\n2\r\nde\r\n0\r\nX-Trailer: 1\r\n\r\n";
        assert_eq!(get(chunked.to_vec(), 100).await.0.unwrap(), b"abcde");
        let eof = b"HTTP/1.0 200 OK\r\n\r\nuntil the end";
        assert_eq!(get(eof.to_vec(), 100).await.0.unwrap(), b"until the end");
        // Chunked wins over a Content-Length (RFC 9112 6.3).
        let both = b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n\
                     2\r\nok\r\n0\r\n\r\n";
        assert_eq!(get(both.to_vec(), 100).await.0.unwrap(), b"ok");
    }

    #[tokio::test]
    async fn bad_responses_are_errors() {
        let cases: &[(&[u8], &str)] = &[
            (
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
                "404",
            ),
            (b"HTTP/1.1 200 OK\r\nContent-Length: 101\r\n\r\n", "bigger"),
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nab",
                "ended after",
            ),
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: x\r\n\r\n",
                "Content-Length",
            ),
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
                "conflicting",
            ),
            (
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n",
                "unsupported",
            ),
            (
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
                "chunk size",
            ),
            (
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabcX\r\n0\r\n\r\n",
                "line break",
            ),
            (
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nab",
                "cut short",
            ),
            (
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n65\r\n",
                "bigger",
            ),
            (
                b"HTTP/1.1 200 OK\r\nContent-Le",
                "before the response head ended",
            ),
            (b"garbage\r\n\r\n", "malformed"),
        ];
        for (response, want) in cases {
            let (res, _) = get(response.to_vec(), 100).await;
            let err = format!("{:#}", res.expect_err(&String::from_utf8_lossy(response)));
            assert!(
                err.contains(want),
                "{err:?} for {:?}",
                String::from_utf8_lossy(response)
            );
        }
        // Exactly at the limit is fine; one byte past it is not.
        let head = b"HTTP/1.1 200 OK\r\n\r\n";
        let (res, _) = get([&head[..], &[b'x'; 101]].concat(), 100).await;
        assert!(format!("{:#}", res.unwrap_err()).contains("bigger"));
        let (res, _) = get([&head[..], &[b'x'; 100]].concat(), 100).await;
        assert_eq!(res.unwrap().len(), 100);
    }

    #[tokio::test]
    async fn an_endless_head_is_cut_off() {
        let mut response = b"HTTP/1.1 200 OK\r\n".to_vec();
        while response.len() < MAX_HEAD_LEN + 100 {
            response.extend_from_slice(b"X-Filler: 0123456789\r\n");
        }
        let (res, _) = get(response, 100).await;
        assert!(format!("{:#}", res.unwrap_err()).contains("longer than"));
    }

    #[tokio::test]
    async fn control_characters_are_never_sent() {
        let (ours, _theirs) = tokio::io::duplex(1024);
        let err = http_get(Box::new(ours), "a.i2p\r\nX: y", "/", "ua", 10)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("control character"));
    }

    fn parse(bytes: &[u8]) -> anyhow::Result<I2pTrackerResponse<'_>> {
        Ok(bencode::from_bytes::<I2pTrackerResponse>(bytes).map_err(|e| e.into_kind())?)
    }

    #[test]
    fn compact_peers_are_hashes() {
        let t = FakeTransport::default();
        let mut body = b"d8:intervali60e5:peers64:".to_vec();
        body.extend_from_slice(&[1; 32]);
        body.extend_from_slice(&[2; 32]);
        body.push(b'e');
        let r = parse(&body).unwrap();
        let addrs = r.peers.addrs(&t).unwrap();
        assert_eq!(addrs, vec![synthetic_addr(1), synthetic_addr(2)]);
        assert_eq!(t.addr_of(I2pHash([2; 32])), synthetic_addr(2));

        let r = parse(b"d5:peers33:012345678901234567890123456789012e").unwrap();
        assert!(r.peers.addrs(&t).is_err());
        let r = parse(b"d5:peers0:e").unwrap();
        assert!(r.peers.addrs(&t).unwrap().is_empty());
    }

    #[test]
    fn dictionary_peers_are_destinations() {
        let t = FakeTransport::default();
        let d1 = dest(1);
        let d2 = dest(2);
        let b32 = I2pHash([3; 32]).b32_name();
        let entries = [
            format!("{}.i2p", d1.to_i2p_base64()),
            d2.to_i2p_base64(),
            b32.to_uppercase(),
            "1.2.3.4".to_owned(),
            "not a destination.i2p".to_owned(),
        ];
        let mut body = b"d5:peersl".to_vec();
        for e in &entries {
            body.extend_from_slice(format!("d2:ip{}:{}4:porti6881ee", e.len(), e).as_bytes());
        }
        body.extend_from_slice(b"ee");
        let r = parse(&body).unwrap();
        let addrs = r.peers.addrs(&t).unwrap();
        assert_eq!(addrs.len(), 3, "the address and the junk are skipped");
        assert!(addrs.iter().all(is_synthetic));
        assert_eq!(addrs[0], t.addr_of(d1.hash()));
        assert_eq!(addrs[1], t.addr_of(d2.hash()));
        assert_eq!(addrs[2], t.addr_of(I2pHash([3; 32])));
    }

    #[test]
    fn peers_per_response_are_capped() {
        let t = FakeTransport::default();
        let n = MAX_PEERS_PER_RESPONSE + 10;
        let mut body = format!("d5:peers{}:", n * 32).into_bytes();
        for i in 0..n {
            let mut h = [0u8; 32];
            h[..8].copy_from_slice(&(i as u64).to_be_bytes());
            body.extend_from_slice(&h);
        }
        body.push(b'e');
        let r = parse(&body).unwrap();
        assert_eq!(r.peers.addrs(&t).unwrap().len(), MAX_PEERS_PER_RESPONSE);

        let d = dest(1).to_i2p_base64();
        let mut body = b"d5:peersl".to_vec();
        for _ in 0..n {
            body.extend_from_slice(format!("d2:ip{}:{d}e", d.len()).as_bytes());
        }
        body.extend_from_slice(b"ee");
        let r = parse(&body).unwrap();
        assert_eq!(r.peers.addrs(&t).unwrap().len(), MAX_PEERS_PER_RESPONSE);
    }
}
