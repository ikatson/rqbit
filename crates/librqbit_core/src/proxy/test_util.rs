//! In-process fake proxies for tests: SOCKS5 (CONNECT and UDP ASSOCIATE, optional RFC 1929
//! authentication), SOCKS4/4a and HTTP `CONNECT` (optional Basic authentication).
//!
//! A [`FakeProxy`] records what it relayed and from which of its own sockets, so a test can
//! check that a destination was only ever reached through the proxy. It can be told to refuse
//! every request, be stopped (the port then refuses connections and every relayed connection
//! and association dies) and be started again on the same port.

#![allow(clippy::unwrap_used)]

use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
};

use parking_lot::Mutex;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

/// The protocol a [`FakeProxy`] speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeProxyKind {
    /// SOCKS5; the URL scheme is `socks5h` when [`FakeProxyOptions::remote_dns`] is set.
    Socks5,
    /// SOCKS4 and SOCKS4a; the URL scheme is `socks4a` when [`FakeProxyOptions::remote_dns`] is
    /// set.
    Socks4,
    /// HTTP `CONNECT`.
    Http,
}

/// How a [`FakeProxy`] behaves.
#[derive(Debug, Clone)]
pub struct FakeProxyOptions {
    /// The protocol.
    pub kind: FakeProxyKind,
    /// The credentials the proxy requires: user name and password for SOCKS5 and HTTP, the user
    /// id (the first element) for SOCKS4.
    pub credentials: Option<(String, String)>,
    /// Whether the SOCKS5 proxy accepts UDP ASSOCIATE.
    pub udp: bool,
    /// Whether the SOCKS5 proxy answers UDP ASSOCIATE with the unspecified address (`0.0.0.0`),
    /// meaning "send to the proxy host", instead of the address of the relay socket.
    pub unspecified_relay_addr: bool,
    /// Whether the URL tells the client to let the proxy resolve host names.
    pub remote_dns: bool,
    /// Whether the SOCKS5 relay gives the source of an answer by name (`ATYP=3`) when the
    /// client named the destination, instead of giving its address.
    pub name_sources: bool,
    /// Whether every request is refused.
    pub refuse: bool,
    /// The address the proxy listens on (`127.0.0.1` by default).
    pub listen_ip: IpAddr,
}

impl FakeProxyOptions {
    /// A proxy of `kind` without authentication that relays everything, UDP included.
    pub fn new(kind: FakeProxyKind) -> Self {
        Self {
            kind,
            credentials: None,
            udp: true,
            unspecified_relay_addr: false,
            remote_dns: false,
            name_sources: false,
            refuse: false,
            listen_ip: Ipv4Addr::LOCALHOST.into(),
        }
    }
}

/// A TCP connection the proxy made for a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayedConnection {
    /// The destination as the client asked for it: `ip:port`, or `host:port` for a host name.
    pub target: String,
    /// The local address of the proxy's connection to the destination: what the destination
    /// sees as the source.
    pub from: SocketAddr,
}

/// A datagram the proxy relayed from a client to a destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayedDatagram {
    /// The destination as the client asked for it: `ip:port`, or `host:port` for a host name.
    pub target: String,
    /// The relay socket it was sent from: what the destination sees as the source.
    pub from: SocketAddr,
    /// The payload.
    pub payload: Vec<u8>,
}

#[derive(Default)]
struct Log {
    connections: Vec<RelayedConnection>,
    datagrams: Vec<RelayedDatagram>,
    datagrams_to_client: usize,
    fragments_dropped: usize,
    associations: usize,
    auth_failures: usize,
    refused: usize,
    accepted: usize,
}

struct Shared {
    opts: FakeProxyOptions,
    log: Mutex<Log>,
    /// Cancelled to drop only the UDP associations (their control connections).
    udp_token: Mutex<CancellationToken>,
}

/// A fake proxy on `127.0.0.1`. See the [module documentation](self).
pub struct FakeProxy {
    addr: SocketAddr,
    shared: Arc<Shared>,
    /// The accept loop, and the token that ends it and every session when the proxy stops.
    running: Mutex<Option<(CancellationToken, JoinHandle<()>)>>,
}

impl Drop for FakeProxy {
    fn drop(&mut self) {
        if let Some((token, _)) = self.running.lock().take() {
            token.cancel();
        }
    }
}

impl FakeProxy {
    /// Starts a proxy on a free port of [`FakeProxyOptions::listen_ip`].
    pub async fn start(opts: FakeProxyOptions) -> Self {
        let listener = TcpListener::bind((opts.listen_ip, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let this = Self {
            addr,
            shared: Arc::new(Shared {
                opts,
                log: Default::default(),
                udp_token: Default::default(),
            }),
            running: Mutex::new(None),
        };
        this.run(listener);
        this
    }

    fn run(&self, listener: TcpListener) {
        let token = CancellationToken::new();
        let shared = self.shared.clone();
        let task_token = token.clone();
        let task = tokio::spawn(async move {
            let token = task_token;
            loop {
                let (stream, _) = tokio::select! {
                    biased;
                    _ = token.cancelled() => return,
                    r = listener.accept() => match r {
                        Ok(r) => r,
                        Err(_) => continue,
                    },
                };
                shared.log.lock().accepted += 1;
                let shared = shared.clone();
                let token = token.clone();
                let udp_token = shared.udp_token.lock().clone();
                tokio::spawn(async move {
                    tokio::select! {
                        _ = token.cancelled() => {}
                        _ = serve(stream, &shared, &udp_token) => {}
                    }
                });
            }
        });
        *self.running.lock() = Some((token, task));
    }

    /// The address the proxy listens on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The proxy URL, with the credentials (percent-encoded).
    pub fn url(&self) -> String {
        let opts = &self.shared.opts;
        let credentials = match (&opts.credentials, opts.kind) {
            (None, _) => String::new(),
            (Some((user, _)), FakeProxyKind::Socks4) => format!("{}@", percent_encode(user)),
            (Some((user, pass)), _) => {
                format!("{}:{}@", percent_encode(user), percent_encode(pass))
            }
        };
        format!("{}://{credentials}{}", self.scheme(), self.addr)
    }

    /// [`Self::url`] with `host` in place of the IP address the proxy listens on, e.g. a name
    /// that resolves to it.
    pub fn url_with_host(&self, host: &str) -> String {
        let url = self.url();
        let ip = match self.addr {
            SocketAddr::V4(a) => a.ip().to_string(),
            SocketAddr::V6(a) => format!("[{}]", a.ip()),
        };
        match url.rsplit_once(&format!("{ip}:")) {
            Some((before, port)) => format!("{before}{host}:{port}"),
            None => url,
        }
    }

    /// The proxy URL with other credentials (for wrong-password tests).
    pub fn url_with_credentials(&self, user: &str, pass: &str) -> String {
        format!(
            "{}://{}:{}@{}",
            self.scheme(),
            percent_encode(user),
            percent_encode(pass),
            self.addr
        )
    }

    fn scheme(&self) -> &'static str {
        let opts = &self.shared.opts;
        match (opts.kind, opts.remote_dns) {
            (FakeProxyKind::Socks5, false) => "socks5",
            (FakeProxyKind::Socks5, true) => "socks5h",
            (FakeProxyKind::Socks4, false) => "socks4",
            (FakeProxyKind::Socks4, true) => "socks4a",
            (FakeProxyKind::Http, _) => "http",
        }
    }

    /// Stops the proxy: the port refuses connections, and every relayed connection and UDP
    /// association ends.
    pub async fn stop(&self) {
        let running = self.running.lock().take();
        if let Some((token, task)) = running {
            token.cancel();
            let _ = task.await;
        }
    }

    /// Starts a stopped proxy again on the same port.
    pub async fn restart(&self) {
        self.stop().await;
        let listener = TcpListener::bind(self.addr).await.unwrap();
        self.run(listener);
    }

    /// Closes the control connection of every UDP association (which ends them) and keeps
    /// accepting new ones.
    pub fn drop_udp_associations(&self) {
        let mut token = self.shared.udp_token.lock();
        token.cancel();
        *token = CancellationToken::new();
    }

    /// The TCP connections relayed so far.
    pub fn connections(&self) -> Vec<RelayedConnection> {
        self.shared.log.lock().connections.clone()
    }

    /// The datagrams relayed from clients so far.
    pub fn datagrams(&self) -> Vec<RelayedDatagram> {
        self.shared.log.lock().datagrams.clone()
    }

    /// How many datagrams were relayed back to clients.
    pub fn datagrams_to_client(&self) -> usize {
        self.shared.log.lock().datagrams_to_client
    }

    /// How many fragmented datagrams (`FRAG != 0`) were dropped.
    pub fn fragments_dropped(&self) -> usize {
        self.shared.log.lock().fragments_dropped
    }

    /// How many UDP associations were established.
    pub fn associations(&self) -> usize {
        self.shared.log.lock().associations
    }

    /// How many clients presented wrong or no credentials.
    pub fn auth_failures(&self) -> usize {
        self.shared.log.lock().auth_failures
    }

    /// How many requests were refused because of [`FakeProxyOptions::refuse`].
    pub fn refused(&self) -> usize {
        self.shared.log.lock().refused
    }

    /// How many TCP connections the proxy accepted.
    pub fn accepted(&self) -> usize {
        self.shared.log.lock().accepted
    }

    /// The source addresses the destinations saw: the proxy's outgoing TCP connections and
    /// its UDP relay sockets.
    pub fn egress_addrs(&self) -> HashSet<SocketAddr> {
        let log = self.shared.log.lock();
        log.connections
            .iter()
            .map(|c| c.from)
            .chain(log.datagrams.iter().map(|d| d.from))
            .collect()
    }
}

async fn serve(mut stream: TcpStream, shared: &Shared, udp_token: &CancellationToken) {
    let _ = match shared.opts.kind {
        FakeProxyKind::Socks5 => serve_socks5(&mut stream, shared, udp_token).await,
        FakeProxyKind::Socks4 => serve_socks4(&mut stream, shared).await,
        FakeProxyKind::Http => serve_http(&mut stream, shared).await,
    };
}

type R<T> = std::io::Result<T>;

fn bad(what: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, what)
}

async fn read_vec<S: AsyncRead + Unpin>(s: &mut S, len: usize) -> R<Vec<u8>> {
    let mut v = vec![0u8; len];
    s.read_exact(&mut v).await?;
    Ok(v)
}

async fn read_until_nul<S: AsyncRead + Unpin>(s: &mut S) -> R<Vec<u8>> {
    let mut v = Vec::new();
    loop {
        let b = s.read_u8().await?;
        if b == 0 {
            return Ok(v);
        }
        if v.len() > 1024 {
            return Err(bad("string too long"));
        }
        v.push(b);
    }
}

/// Resolves a requested destination; the proxy only ever connects to loopback in tests, where
/// every fake server listens on IPv4, so an IPv4 address is preferred (`localhost` may resolve
/// to `::1` first).
async fn resolve(target: &str) -> R<SocketAddr> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(target).await?.collect();
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| addrs.first())
        .copied()
        .ok_or_else(|| bad("host not found"))
}

async fn connect_and_log(target: &str, shared: &Shared) -> R<TcpStream> {
    let addr = resolve(target).await?;
    let out = TcpStream::connect(addr).await?;
    shared.log.lock().connections.push(RelayedConnection {
        target: target.to_owned(),
        from: out.local_addr()?,
    });
    Ok(out)
}

fn socks5_addr(addr: SocketAddr) -> Vec<u8> {
    let mut v = Vec::with_capacity(19);
    match addr.ip() {
        IpAddr::V4(ip) => {
            v.push(1);
            v.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            v.push(4);
            v.extend_from_slice(&ip.octets());
        }
    }
    v.extend_from_slice(&addr.port().to_be_bytes());
    v
}

async fn socks5_reply(stream: &mut TcpStream, rep: u8, bound: SocketAddr) -> R<()> {
    let mut reply = vec![5, rep, 0];
    reply.extend_from_slice(&socks5_addr(bound));
    stream.write_all(&reply).await
}

/// Reads `ATYP DST.ADDR DST.PORT` from a SOCKS5 request.
async fn read_socks5_target<S: AsyncRead + Unpin>(s: &mut S) -> R<String> {
    let atyp = s.read_u8().await?;
    let host = match atyp {
        1 => IpAddr::from(<[u8; 4]>::try_from(read_vec(s, 4).await?).unwrap()).to_string(),
        4 => format!(
            "[{}]",
            Ipv6Addr::from(<[u8; 16]>::try_from(read_vec(s, 16).await?).unwrap())
        ),
        3 => {
            let len = s.read_u8().await?;
            String::from_utf8(read_vec(s, len.into()).await?).map_err(|_| bad("domain"))?
        }
        _ => return Err(bad("atyp")),
    };
    let port = s.read_u16().await?;
    Ok(format!("{host}:{port}"))
}

/// Parses the header of a SOCKS5 UDP request: `(FRAG, target, header length)`.
fn parse_udp_header(d: &[u8]) -> Option<(u8, String, usize)> {
    let frag = *d.get(2)?;
    let (host, rest) = match *d.get(3)? {
        1 => {
            let ip: [u8; 4] = d.get(4..8)?.try_into().ok()?;
            (IpAddr::from(ip).to_string(), 8)
        }
        4 => {
            let ip: [u8; 16] = d.get(4..20)?.try_into().ok()?;
            (format!("[{}]", Ipv6Addr::from(ip)), 20)
        }
        3 => {
            let len = usize::from(*d.get(4)?);
            let name = std::str::from_utf8(d.get(5..5 + len)?).ok()?;
            (name.to_owned(), 5 + len)
        }
        _ => return None,
    };
    let port = u16::from_be_bytes(d.get(rest..rest + 2)?.try_into().ok()?);
    Some((frag, format!("{host}:{port}"), rest + 2))
}

async fn serve_socks5(
    stream: &mut TcpStream,
    shared: &Shared,
    udp_token: &CancellationToken,
) -> R<()> {
    let opts = &shared.opts;
    if stream.read_u8().await? != 5 {
        return Err(bad("version"));
    }
    let n = stream.read_u8().await?;
    let methods = read_vec(stream, n.into()).await?;
    let wanted = if opts.credentials.is_some() { 2 } else { 0 };
    if !methods.contains(&wanted) {
        if opts.credentials.is_some() {
            shared.log.lock().auth_failures += 1;
        }
        stream.write_all(&[5, 0xff]).await?;
        return Ok(());
    }
    stream.write_all(&[5, wanted]).await?;
    if let Some((user, pass)) = opts.credentials.as_ref() {
        if stream.read_u8().await? != 1 {
            return Err(bad("auth version"));
        }
        let ulen = stream.read_u8().await?;
        let u = read_vec(stream, ulen.into()).await?;
        let plen = stream.read_u8().await?;
        let p = read_vec(stream, plen.into()).await?;
        if u != user.as_bytes() || p != pass.as_bytes() {
            shared.log.lock().auth_failures += 1;
            stream.write_all(&[1, 1]).await?;
            return Ok(());
        }
        stream.write_all(&[1, 0]).await?;
    }

    let mut head = [0u8; 3];
    stream.read_exact(&mut head).await?;
    if head[0] != 5 {
        return Err(bad("version"));
    }
    let target = read_socks5_target(stream).await?;
    let unspecified: SocketAddr = (Ipv4Addr::UNSPECIFIED, 0).into();
    if opts.refuse {
        shared.log.lock().refused += 1;
        return socks5_reply(stream, 2, unspecified).await;
    }
    match head[1] {
        1 => {
            let out = match connect_and_log(&target, shared).await {
                Ok(out) => out,
                Err(_) => return socks5_reply(stream, 5, unspecified).await,
            };
            socks5_reply(stream, 0, out.local_addr()?).await?;
            relay_tcp(stream, out).await
        }
        3 if opts.udp => serve_udp_association(stream, shared, udp_token).await,
        _ => socks5_reply(stream, 7, unspecified).await,
    }
}

async fn serve_udp_association(
    control: &mut TcpStream,
    shared: &Shared,
    udp_token: &CancellationToken,
) -> R<()> {
    let relay = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let relay_addr = relay.local_addr()?;
    let bound = if shared.opts.unspecified_relay_addr {
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, relay_addr.port()))
    } else {
        relay_addr
    };
    socks5_reply(control, 0, bound).await?;
    shared.log.lock().associations += 1;

    let mut client: Option<SocketAddr> = None;
    // The destinations the client gave by name, by address.
    let mut names: HashMap<SocketAddr, String> = HashMap::new();
    let mut buf = vec![0u8; 65536];
    let mut control_buf = [0u8; 64];
    loop {
        tokio::select! {
            _ = udp_token.cancelled() => return Ok(()),
            r = control.read(&mut control_buf) => {
                // The association lives as long as the control connection.
                if matches!(r, Ok(0) | Err(_)) {
                    return Ok(());
                }
            }
            r = relay.recv_from(&mut buf) => {
                let (len, from) = r?;
                let d = &buf[..len];
                // The first datagram comes from the client: destinations only answer it.
                let client = *client.get_or_insert(from);
                if from == client {
                    let Some((frag, target, header_len)) = parse_udp_header(d) else {
                        continue;
                    };
                    if frag != 0 {
                        shared.log.lock().fragments_dropped += 1;
                        continue;
                    }
                    let Ok(dest) = resolve(&target).await else {
                        continue;
                    };
                    if target.parse::<SocketAddr>().is_err() {
                        names.insert(dest, target.clone());
                    }
                    let payload = d[header_len..].to_vec();
                    let _ = relay.send_to(&payload, dest).await;
                    shared.log.lock().datagrams.push(RelayedDatagram {
                        target,
                        from: relay_addr,
                        payload,
                    });
                } else {
                    let mut out = vec![0, 0, 0];
                    let name = names
                        .get(&from)
                        .filter(|_| shared.opts.name_sources)
                        .and_then(|t| t.rsplit_once(':'));
                    match name {
                        Some((host, _)) => {
                            out.push(3);
                            out.push(u8::try_from(host.len()).map_err(|_| bad("name"))?);
                            out.extend_from_slice(host.as_bytes());
                            out.extend_from_slice(&from.port().to_be_bytes());
                        }
                        None => out.extend_from_slice(&socks5_addr(from)),
                    }
                    out.extend_from_slice(d);
                    let _ = relay.send_to(&out, client).await;
                    shared.log.lock().datagrams_to_client += 1;
                }
            }
        }
    }
}

async fn relay_tcp(client: &mut TcpStream, mut out: TcpStream) -> R<()> {
    tokio::io::copy_bidirectional(client, &mut out).await?;
    Ok(())
}

async fn serve_socks4(stream: &mut TcpStream, shared: &Shared) -> R<()> {
    let mut head = [0u8; 8];
    stream.read_exact(&mut head).await?;
    if head[0] != 4 || head[1] != 1 {
        return Err(bad("request"));
    }
    let port = u16::from_be_bytes([head[2], head[3]]);
    let ip = Ipv4Addr::new(head[4], head[5], head[6], head[7]);
    let user = read_until_nul(stream).await?;
    // SOCKS4a: 0.0.0.x with x != 0, the host name follows the user id.
    let target = if head[4..7] == [0, 0, 0] && head[7] != 0 {
        let host = String::from_utf8(read_until_nul(stream).await?).map_err(|_| bad("host"))?;
        format!("{host}:{port}")
    } else {
        format!("{ip}:{port}")
    };
    let reply = |code: u8| [0, code, 0, 0, 0, 0, 0, 0];
    if let Some((want, _)) = shared.opts.credentials.as_ref()
        && user != want.as_bytes()
    {
        shared.log.lock().auth_failures += 1;
        return stream.write_all(&reply(0x5b)).await;
    }
    if shared.opts.refuse {
        shared.log.lock().refused += 1;
        return stream.write_all(&reply(0x5b)).await;
    }
    let out = match connect_and_log(&target, shared).await {
        Ok(out) => out,
        Err(_) => return stream.write_all(&reply(0x5b)).await,
    };
    stream.write_all(&reply(0x5a)).await?;
    relay_tcp(stream, out).await
}

/// `CONNECT host:port` tunnels, and plain requests in absolute form (`GET http://host/path`,
/// what an HTTP client sends for `http://` URLs) forwarded in origin form.
async fn serve_http(stream: &mut TcpStream, shared: &Shared) -> R<()> {
    let mut buf = Vec::new();
    let (target, auth, forward) = loop {
        let mut chunk = [0u8; 1024];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut req = httparse::Request::new(&mut headers);
        match req.parse(&buf).map_err(|_| bad("http"))? {
            httparse::Status::Partial => continue,
            httparse::Status::Complete(_) => {
                let auth = req
                    .headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case("proxy-authorization"))
                    .map(|h| h.value.to_vec());
                let path = req.path.unwrap_or_default();
                let method = req.method.unwrap_or_default();
                if method == "CONNECT" {
                    break (path.to_owned(), auth, None);
                }
                let Some(rest) = path.strip_prefix("http://") else {
                    stream
                        .write_all(b"HTTP/1.1 405 Method Not Allowed\r\n\r\n")
                        .await?;
                    return Ok(());
                };
                let (authority, origin) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
                let has_port = authority
                    .rsplit_once(':')
                    .is_some_and(|(_, p)| !p.is_empty() && !p.contains(']'));
                let target = if has_port {
                    authority.to_owned()
                } else {
                    format!("{authority}:80")
                };
                let origin = if origin.is_empty() { "/" } else { origin };
                // The request line rewritten, then the headers and whatever followed them.
                let line_end = buf
                    .windows(2)
                    .position(|w| w == b"\r\n")
                    .ok_or(bad("http"))?;
                let mut forward = format!("{method} {origin} HTTP/1.1").into_bytes();
                forward.extend_from_slice(&buf[line_end..]);
                break (target, auth, Some(forward));
            }
        }
    };
    if let Some((user, pass)) = shared.opts.credentials.as_ref() {
        let want = format!(
            "Basic {}",
            data_encoding::BASE64.encode(format!("{user}:{pass}").as_bytes())
        );
        if auth.as_deref() != Some(want.as_bytes()) {
            shared.log.lock().auth_failures += 1;
            stream
                .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"test\"\r\n\r\n")
                .await?;
            return Ok(());
        }
    }
    if shared.opts.refuse {
        shared.log.lock().refused += 1;
        stream.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n").await?;
        return Ok(());
    }
    let mut out = match connect_and_log(&target, shared).await {
        Ok(out) => out,
        Err(_) => {
            stream
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n")
                .await?;
            return Ok(());
        }
    };
    match forward {
        Some(request) => out.write_all(&request).await?,
        None => {
            stream
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await?
        }
    }
    relay_tcp(stream, out).await
}

/// Percent-encodes everything but unreserved characters, for the user info of a URL.
fn percent_encode(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}
