//! Datagrams through a SOCKS5 proxy with UDP ASSOCIATE (RFC 1928 §7).
//!
//! An association is a TCP control connection to the proxy plus a local UDP socket. Every
//! datagram is sent to the relay address the proxy gave, prefixed with
//! `RSV RSV FRAG ATYP DST.ADDR DST.PORT`; the relay forwards it from its own socket and wraps
//! the answers the same way. The association lives as long as the control connection: when that
//! drops (or the proxy is down), [`Socks5UdpAssociation`] establishes a new one with an
//! exponential backoff. In the meantime sending fails; nothing is ever sent directly.
//!
//! Nothing is ever written to the control connection, so a proxy host that vanishes without
//! closing it (power loss, a NAT forgetting the connection) can't be noticed there. The relay is
//! watched instead: when datagrams keep going out and nothing at all comes back for a while,
//! the association is taken for dead and rebuilt.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
    sync::{Notify, watch},
};
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::{debug, info, trace, warn};

use super::{ProxyConfig, ProxyError, TargetAddr};

/// The largest UDP payload over IPv4: 65535 minus the IP (20) and UDP (8) headers.
pub const MAX_UDP_PAYLOAD: usize = 65_507;

/// The shortest SOCKS5 UDP request header (an IPv4 address).
pub const SOCKS5_UDP_HEADER_MIN: usize = 10;

/// The longest SOCKS5 UDP request header (a 255-byte domain name). A receive buffer must be this
/// much larger than the largest datagram expected, as the header is received into it too.
pub const SOCKS5_UDP_HEADER_MAX: usize = 262;

/// The longest a connection attempt (TCP connect, authentication, UDP ASSOCIATE) may take.
const ASSOCIATE_TIMEOUT: Duration = Duration::from_secs(10);
const MIN_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// The wait after the proxy refused UDP ASSOCIATE or the credentials: asking again soon would
/// get the same answer.
const REFUSED_BACKOFF: Duration = Duration::from_secs(300);
/// An association that lasted this long was healthy: the next attempt starts from
/// [`MIN_BACKOFF`] again.
const STABLE_ASSOCIATION: Duration = Duration::from_secs(10);
/// The same failure is logged as a warning again after this long; repeats in between are
/// debug messages.
const WARN_AGAIN_AFTER: Duration = Duration::from_secs(3600);
/// An association is taken for dead when at least [`SILENT_SENDS`] datagrams went out and
/// the relay sent nothing back since the first of them, [`SILENT_TIMEOUT`] ago or more.
const SILENT_SENDS: u32 = 8;
#[cfg(not(test))]
const SILENT_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(test)]
const SILENT_TIMEOUT: Duration = Duration::from_millis(500);
#[cfg(not(test))]
const SILENCE_CHECK: Duration = Duration::from_secs(5);
#[cfg(test)]
const SILENCE_CHECK: Duration = Duration::from_millis(50);

/// One established association.
#[derive(Debug)]
struct Association {
    socket: UdpSocket,
    /// Where the proxy relays from: datagrams are sent there, and only datagrams from there are
    /// accepted.
    relay: SocketAddr,
    /// Scratch space to put the header in front of a payload.
    send_buf: tokio::sync::Mutex<Vec<u8>>,
    created: Instant,
    /// Datagrams sent since the relay last sent one.
    unanswered: AtomicU32,
    /// When the first of those went out, in milliseconds since `created`.
    first_unanswered_ms: AtomicU64,
}

impl Association {
    fn new(socket: UdpSocket, relay: SocketAddr) -> Self {
        Self {
            socket,
            relay,
            send_buf: tokio::sync::Mutex::new(Vec::new()),
            created: Instant::now(),
            unanswered: AtomicU32::new(0),
            first_unanswered_ms: AtomicU64::new(0),
        }
    }

    fn sent(&self) {
        if self.unanswered.fetch_add(1, Ordering::Relaxed) == 0 {
            let ms = u64::try_from(self.created.elapsed().as_millis()).unwrap_or(u64::MAX);
            self.first_unanswered_ms.store(ms, Ordering::Relaxed);
        }
    }

    fn answered(&self) {
        self.unanswered.store(0, Ordering::Relaxed);
    }

    /// Whether the relay looks dead (see [`SILENT_SENDS`]).
    fn is_silent(&self) -> bool {
        if self.unanswered.load(Ordering::Relaxed) < SILENT_SENDS {
            return false;
        }
        let first = Duration::from_millis(self.first_unanswered_ms.load(Ordering::Relaxed));
        self.created.elapsed().saturating_sub(first) >= SILENT_TIMEOUT
    }
}

#[derive(Debug, Clone)]
enum State {
    /// Nothing was sent yet; the proxy is not contacted before the first use.
    Idle,
    /// An attempt to establish an association is in progress.
    Connecting,
    Up(Arc<Association>),
    /// The last attempt failed or the association ended; the next attempt is scheduled.
    Down(Arc<str>),
    /// The association was shut down for good (cancelled or dropped).
    Closed,
}

#[derive(Debug)]
struct Inner {
    proxy: ProxyConfig,
    state: watch::Sender<State>,
    /// Signalled by the first use.
    wanted: Notify,
}

/// A UDP "socket" whose datagrams go through a SOCKS5 proxy (UDP ASSOCIATE).
///
/// Send and receive can be called concurrently from several tasks. The proxy is contacted on
/// the first [`send_to`](Self::send_to); after that the
/// association is kept up, and re-established with backoff whenever it ends. The association
/// shuts down when this value is dropped or the cancellation token given to
/// [`new`](Self::new) is cancelled.
///
/// Its sockets are not bound to a network device: they follow the routing table, like the
/// SOCKS TCP connections to the proxy.
#[derive(Debug)]
pub struct Socks5UdpAssociation {
    inner: Arc<Inner>,
    _stop: DropGuard,
}

impl Socks5UdpAssociation {
    /// Creates the association for `proxy`. Must be called inside a Tokio runtime.
    ///
    /// Fails with [`ProxyError::UdpNotSupported`] when `proxy` is not a SOCKS5 proxy.
    pub fn new(proxy: ProxyConfig, cancel: &CancellationToken) -> Result<Self, ProxyError> {
        if !proxy.kind().can_relay_udp() {
            return Err(ProxyError::UdpNotSupported(proxy.kind()));
        }
        let inner = Arc::new(Inner {
            proxy,
            state: watch::Sender::new(State::Idle),
            wanted: Notify::new(),
        });
        let token = cancel.child_token();
        tokio::spawn({
            let inner = inner.clone();
            let token = token.clone();
            async move {
                tokio::select! {
                    _ = token.cancelled() => {}
                    _ = maintain(&inner) => {}
                }
                inner.state.send_replace(State::Closed);
            }
        });
        Ok(Self {
            inner,
            _stop: token.drop_guard(),
        })
    }

    /// The proxy this association goes through.
    pub fn proxy(&self) -> &ProxyConfig {
        &self.inner.proxy
    }

    /// Whether an association is established right now.
    pub fn is_up(&self) -> bool {
        matches!(*self.inner.state.borrow(), State::Up(_))
    }

    /// Sends `payload` to `target` through the proxy.
    ///
    /// A [`TargetAddr::Domain`] is sent as a name for the proxy to resolve (`ATYP=3`); whether
    /// to do that or to resolve locally is up to the caller (see
    /// [`ProxyKind::resolves_hostnames`](super::ProxyKind::resolves_hostnames)).
    ///
    /// Waits for an association while the first one, or a new one, is being established; fails
    /// with [`ProxyError::UdpAssociationDown`] when the last attempt failed and the next one is
    /// not due yet.
    pub async fn send_to(&self, payload: &[u8], target: TargetAddr<'_>) -> Result<(), ProxyError> {
        let assoc = self.association().await?;
        let mut buf = assoc.send_buf.lock().await;
        buf.clear();
        write_udp_header(&mut buf, target)?;
        if buf.len() + payload.len() > MAX_UDP_PAYLOAD {
            return Err(ProxyError::DatagramTooLarge);
        }
        buf.extend_from_slice(payload);
        assoc
            .socket
            .send_to(&buf, assoc.relay)
            .await
            .map_err(ProxyError::Io)?;
        assoc.sent();
        Ok(())
    }

    /// Receives the next datagram relayed by the proxy into `buf`, and returns its length and
    /// its source.
    ///
    /// Like [`recv`](Self::recv), but datagrams whose source the relay gave as a host name are
    /// dropped.
    pub async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr), ProxyError> {
        loop {
            match self.recv(buf).await? {
                (len, Some(source)) => return Ok((len, source)),
                (_, None) => debug!("dropping a datagram whose source is a host name"),
            }
        }
    }

    /// Receives the next datagram relayed by the proxy into `buf`, and returns its length and
    /// its source, or `None` when the relay gave the source as a host name (`ATYP=3`, which a
    /// relay may do for a destination that was given by name).
    ///
    /// The SOCKS5 header is received into `buf` too, so `buf` should be
    /// [`SOCKS5_UDP_HEADER_MAX`] bytes longer than the largest datagram expected; a longer
    /// datagram is truncated. Datagrams that don't come from the relay, fragmented ones
    /// (`FRAG != 0`) and ones with a malformed header are dropped. While there is no
    /// association, this waits for one (it doesn't start one: the first
    /// [`send_to`](Self::send_to) does); it fails only when the association is shut down, or
    /// when receiving from the local socket fails.
    pub async fn recv(&self, buf: &mut [u8]) -> Result<(usize, Option<SocketAddr>), ProxyError> {
        let mut rx = self.inner.state.subscribe();
        // Receiving alone doesn't contact the proxy: nothing can arrive before something was
        // sent, and a consumer that never sends (no trackers) leaves the proxy alone.
        loop {
            let assoc = match &*rx.borrow_and_update() {
                State::Up(assoc) => Some(assoc.clone()),
                State::Closed => return Err(ProxyError::UdpAssociationShutDown),
                State::Idle | State::Connecting | State::Down(_) => None,
            };
            let Some(assoc) = assoc else {
                if rx.changed().await.is_err() {
                    return Err(ProxyError::UdpAssociationShutDown);
                }
                continue;
            };
            loop {
                tokio::select! {
                    r = assoc.socket.recv_from(buf) => {
                        let (len, from) = r.map_err(ProxyError::Io)?;
                        // IP and port only: a link-local IPv6 relay comes with a scope id.
                        if from.ip().to_canonical() != assoc.relay.ip().to_canonical()
                            || from.port() != assoc.relay.port()
                        {
                            debug!(%from, "dropping a datagram that does not come from the SOCKS5 relay");
                            continue;
                        }
                        assoc.answered();
                        let datagram = buf.get(..len).unwrap_or_default();
                        match parse_udp_header(datagram) {
                            Ok((header_len, source)) => {
                                buf.copy_within(header_len..len, 0);
                                return Ok((len - header_len, source));
                            }
                            Err(why) => {
                                debug!("dropping a datagram from the SOCKS5 relay: {why}");
                                continue;
                            }
                        }
                    }
                    // The association ended or was replaced.
                    r = rx.changed() => {
                        if r.is_err() {
                            return Err(ProxyError::UdpAssociationShutDown);
                        }
                        break;
                    }
                }
            }
        }
    }

    async fn association(&self) -> Result<Arc<Association>, ProxyError> {
        if let State::Up(assoc) = &*self.inner.state.borrow() {
            return Ok(assoc.clone());
        }
        let mut rx = self.inner.state.subscribe();
        self.inner.wanted.notify_one();
        loop {
            match &*rx.borrow_and_update() {
                State::Up(assoc) => return Ok(assoc.clone()),
                State::Down(why) => return Err(ProxyError::UdpAssociationDown(why.to_string())),
                State::Closed => return Err(ProxyError::UdpAssociationShutDown),
                State::Idle | State::Connecting => {}
            }
            if rx.changed().await.is_err() {
                return Err(ProxyError::UdpAssociationShutDown);
            }
        }
    }
}

/// Keeps an association up from the first use on.
async fn maintain(inner: &Inner) {
    inner.wanted.notified().await;
    let mut backoff = MIN_BACKOFF;
    let mut last_warning: Option<(String, Instant)> = None;
    loop {
        inner.state.send_replace(State::Connecting);
        let attempt = tokio::time::timeout(ASSOCIATE_TIMEOUT, associate(&inner.proxy)).await;
        let (why, wait) = match attempt {
            Ok(Ok((control, assoc))) => {
                info!(proxy = %inner.proxy, relay = %assoc.relay, "SOCKS5 UDP association established");
                let assoc = Arc::new(assoc);
                let started = Instant::now();
                inner.state.send_replace(State::Up(assoc.clone()));
                let why = tokio::select! {
                    why = control_closed(control) => why,
                    why = relay_silent(&assoc) => why,
                };
                if started.elapsed() >= STABLE_ASSOCIATION {
                    backoff = MIN_BACKOFF;
                }
                (format!("the association ended: {why}"), backoff)
            }
            Ok(Err(e)) => {
                // Asking again soon would get the same answer.
                let refused = matches!(
                    e,
                    ProxyError::Socks5Refused(2 | 7)
                        | ProxyError::Socks5AuthenticationFailed
                        | ProxyError::Socks5NoAcceptableMethod
                );
                (
                    e.to_string(),
                    if refused { REFUSED_BACKOFF } else { backoff },
                )
            }
            Err(_) => (
                format!("no UDP association after {ASSOCIATE_TIMEOUT:?}"),
                backoff,
            ),
        };
        let repeated = last_warning
            .as_ref()
            .is_some_and(|(last, at)| *last == why && at.elapsed() < WARN_AGAIN_AFTER);
        if repeated {
            debug!(proxy = %inner.proxy, retry_in = ?wait, "SOCKS5 UDP association down: {why}");
        } else {
            warn!(proxy = %inner.proxy, retry_in = ?wait, "SOCKS5 UDP association down: {why}");
            last_warning = Some((why.clone(), Instant::now()));
        }
        inner.state.send_replace(State::Down(why.into()));
        tokio::time::sleep(wait).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Waits until the proxy closes the control connection, and says why it ended.
async fn control_closed(mut control: TcpStream) -> String {
    let mut buf = [0u8; 64];
    loop {
        match control.read(&mut buf).await {
            Ok(0) => return "the proxy closed the control connection".to_owned(),
            // RFC 1928 gives the control connection no further use; whatever comes is ignored.
            Ok(n) => trace!(n, "ignoring bytes on the SOCKS5 control connection"),
            Err(e) => return format!("error on the control connection: {e}"),
        }
    }
}

/// Waits until the relay looks dead (see [`SILENT_SENDS`]), and says why.
async fn relay_silent(assoc: &Association) -> String {
    let mut check = tokio::time::interval(SILENCE_CHECK);
    loop {
        check.tick().await;
        if assoc.is_silent() {
            return format!(
                "the relay sent nothing back for {SILENT_TIMEOUT:?} while datagrams went out"
            );
        }
    }
}

/// Connects to the proxy, authenticates and asks for a UDP association.
///
/// The proxy host itself is looked up locally, like for every other proxy connection. Neither
/// socket is bound to a network device: like the SOCKS TCP connections to the proxy, the
/// association follows the routing table.
async fn associate(proxy: &ProxyConfig) -> Result<(TcpStream, Association), ProxyError> {
    let mut control = TcpStream::connect((proxy.host(), proxy.port()))
        .await
        .map_err(ProxyError::ConnectToProxy)?;
    socks5_authenticate(&mut control, proxy).await?;

    // DST.ADDR / DST.PORT would be the address we send from as the proxy sees it, which we
    // can't know behind a NAT; all zeros means "unknown" (RFC 1928 §7).
    control
        .write_all(&[5, 3, 0, 1, 0, 0, 0, 0, 0, 0])
        .await
        .map_err(ProxyError::Io)?;
    let bound = read_socks5_reply(&mut control).await?;
    if bound.port() == 0 {
        return Err(ProxyError::Malformed("UDP relay port 0"));
    }
    let proxy_ip = control.peer_addr().map_err(ProxyError::Io)?.ip();
    let relay = relay_address(bound, proxy_ip);
    let bind: SocketAddr = match relay {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let socket = UdpSocket::bind(bind).await.map_err(ProxyError::Io)?;
    Ok((control, Association::new(socket, relay)))
}

/// Where to send datagrams, given the relay address `bound` from the proxy's reply and the
/// address `proxy_ip` the control connection reached the proxy at.
///
/// An unspecified address means "the proxy host" (RFC 1928 §7). A proxy behind a NAT or in a
/// container may also give its own internal address, which can't be the right one when the
/// control connection reached it elsewhere: a loopback address when the proxy is not on this
/// host, or a private one when the proxy is on a public address. Then the proxy host is used
/// with the given port too.
fn relay_address(bound: SocketAddr, proxy_ip: IpAddr) -> SocketAddr {
    let ip = bound.ip().to_canonical();
    let proxy_ip = proxy_ip.to_canonical();
    let internal = ip.is_unspecified()
        || (ip.is_loopback() && !proxy_ip.is_loopback())
        || (is_private(ip) && !is_private(proxy_ip) && !proxy_ip.is_loopback());
    if internal {
        SocketAddr::new(proxy_ip, bound.port())
    } else {
        bound
    }
}

/// Addresses only reachable inside some private network.
fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, ..] = ip.octets();
            // 100.64.0.0/10 is carrier-grade NAT (RFC 6598).
            ip.is_private() || ip.is_link_local() || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(ip) => ip.is_unique_local() || ip.is_unicast_link_local(),
    }
}

fn io_error(e: std::io::Error) -> ProxyError {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        ProxyError::Closed
    } else {
        ProxyError::Io(e)
    }
}

/// Method selection, and RFC 1929 username/password authentication when the proxy has
/// credentials.
async fn socks5_authenticate(
    stream: &mut TcpStream,
    proxy: &ProxyConfig,
) -> Result<(), ProxyError> {
    let credentials = proxy.username().zip(proxy.password());
    let greeting: &[u8] = if credentials.is_some() {
        &[5, 2, 0, 2]
    } else {
        &[5, 1, 0]
    };
    stream.write_all(greeting).await.map_err(ProxyError::Io)?;
    let mut answer = [0u8; 2];
    stream.read_exact(&mut answer).await.map_err(io_error)?;
    if answer[0] != 5 {
        return Err(ProxyError::Malformed("not a SOCKS5 proxy"));
    }
    match (answer[1], credentials) {
        (0, _) => Ok(()),
        (2, Some((user, pass))) => {
            // The parser keeps both at most 255 bytes long.
            let (Ok(ulen), Ok(plen)) = (u8::try_from(user.len()), u8::try_from(pass.len())) else {
                return Err(ProxyError::Socks5AuthenticationFailed);
            };
            let mut request = Vec::with_capacity(3 + user.len() + pass.len());
            request.push(1);
            request.push(ulen);
            request.extend_from_slice(user.as_bytes());
            request.push(plen);
            request.extend_from_slice(pass.as_bytes());
            stream.write_all(&request).await.map_err(ProxyError::Io)?;
            let mut status = [0u8; 2];
            stream.read_exact(&mut status).await.map_err(io_error)?;
            if status[1] != 0 {
                return Err(ProxyError::Socks5AuthenticationFailed);
            }
            Ok(())
        }
        (0xff, _) => Err(ProxyError::Socks5NoAcceptableMethod),
        _ => Err(ProxyError::Malformed(
            "the proxy chose a method that was not offered",
        )),
    }
}

/// Reads `VER REP RSV ATYP BND.ADDR BND.PORT`.
///
/// A `BND.ADDR` given as a host name comes back as the unspecified address, i.e. "the proxy
/// host" (see [`relay_address`]): looking the name up here would bypass the proxy's resolver
/// under `socks5h`, and a name the proxy gives itself rarely resolves from outside anyway.
async fn read_socks5_reply(stream: &mut TcpStream) -> Result<SocketAddr, ProxyError> {
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await.map_err(io_error)?;
    if head[0] != 5 {
        return Err(ProxyError::Malformed("not a SOCKS5 reply"));
    }
    if head[1] != 0 {
        return Err(ProxyError::Socks5Refused(head[1]));
    }
    let ip: IpAddr = match head[3] {
        1 => {
            let mut ip = [0u8; 4];
            stream.read_exact(&mut ip).await.map_err(io_error)?;
            ip.into()
        }
        4 => {
            let mut ip = [0u8; 16];
            stream.read_exact(&mut ip).await.map_err(io_error)?;
            ip.into()
        }
        3 => {
            let len = stream.read_u8().await.map_err(io_error)?;
            let mut name = [0u8; 255];
            let name = name
                .get_mut(..usize::from(len))
                .ok_or(ProxyError::Malformed("relay host name too long"))?;
            stream.read_exact(name).await.map_err(io_error)?;
            debug!(
                name = %String::from_utf8_lossy(name),
                "the SOCKS5 relay is given by name: using the proxy host"
            );
            Ipv4Addr::UNSPECIFIED.into()
        }
        _ => return Err(ProxyError::Malformed("unknown address type")),
    };
    let port = stream.read_u16().await.map_err(io_error)?;
    Ok(SocketAddr::new(ip, port))
}

/// Appends `RSV RSV FRAG ATYP DST.ADDR DST.PORT` for `target` to `buf`.
fn write_udp_header(buf: &mut Vec<u8>, target: TargetAddr<'_>) -> Result<(), ProxyError> {
    buf.extend_from_slice(&[0, 0, 0]);
    let port = match target {
        TargetAddr::Ip(addr) => {
            match addr.ip() {
                IpAddr::V4(ip) => {
                    buf.push(1);
                    buf.extend_from_slice(&ip.octets());
                }
                // A v4-mapped address goes out as IPv4, which every proxy can relay.
                IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
                    Some(ip) => {
                        buf.push(1);
                        buf.extend_from_slice(&ip.octets());
                    }
                    None => {
                        buf.push(4);
                        buf.extend_from_slice(&ip.octets());
                    }
                },
            }
            addr.port()
        }
        TargetAddr::Domain(name, port) => {
            let len = u8::try_from(name.len()).map_err(|_| ProxyError::DomainTooLong)?;
            buf.push(3);
            buf.push(len);
            buf.extend_from_slice(name.as_bytes());
            port
        }
    };
    buf.extend_from_slice(&port.to_be_bytes());
    Ok(())
}

/// Parses the header of a datagram from the relay: `(header length, source)`, where the source
/// is `None` when the relay gave it as a host name (`ATYP=3`).
fn parse_udp_header(d: &[u8]) -> Result<(usize, Option<SocketAddr>), &'static str> {
    let [_, _, frag, atyp, rest @ ..] = d else {
        return Err("truncated header");
    };
    if *frag != 0 {
        return Err("fragmented datagram");
    }
    let (source, len) = match atyp {
        1 => match rest {
            [a, b, c, d, p0, p1, ..] => {
                let addr = SocketAddrV4::new(
                    Ipv4Addr::new(*a, *b, *c, *d),
                    u16::from_be_bytes([*p0, *p1]),
                );
                (Some(SocketAddr::V4(addr)), SOCKS5_UDP_HEADER_MIN)
            }
            _ => return Err("truncated header"),
        },
        4 => {
            let (Some(ip), Some(port)) = (rest.get(..16), rest.get(16..18)) else {
                return Err("truncated header");
            };
            let ip: [u8; 16] = ip.try_into().map_err(|_| "truncated header")?;
            let port = u16::from_be_bytes([port[0], port[1]]);
            let addr = SocketAddrV6::new(Ipv6Addr::from(ip), port, 0, 0);
            (Some(SocketAddr::V6(addr)), 22)
        }
        3 => {
            let [name_len, rest @ ..] = rest else {
                return Err("truncated header");
            };
            let name_len = usize::from(*name_len);
            if name_len == 0 {
                return Err("empty host name");
            }
            // The name and the port.
            if rest.len() < name_len + 2 {
                return Err("truncated header");
            }
            (None, 4 + 1 + name_len + 2)
        }
        _ => return Err("unknown address type"),
    };
    Ok((len, source))
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};
    use std::time::Duration;

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, UdpSocket},
    };
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::proxy::test_util::{FakeProxy, FakeProxyKind, FakeProxyOptions};

    const WAIT: Duration = Duration::from_secs(20);

    /// A UDP server that sends every datagram back to where it came from, and records the
    /// sources.
    struct Echo {
        addr: SocketAddr,
        sources: Arc<parking_lot::Mutex<Vec<SocketAddr>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Echo {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn echo() -> Echo {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = socket.local_addr().unwrap();
        let sources = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let task = tokio::spawn({
            let sources = sources.clone();
            async move {
                let mut buf = [0u8; 2048];
                while let Ok((len, from)) = socket.recv_from(&mut buf).await {
                    sources.lock().push(from);
                    let _ = socket.send_to(&buf[..len], from).await;
                }
            }
        });
        Echo {
            addr,
            sources,
            task,
        }
    }

    fn association(proxy: &FakeProxy) -> Socks5UdpAssociation {
        Socks5UdpAssociation::new(
            ProxyConfig::parse(&proxy.url()).unwrap(),
            &CancellationToken::new(),
        )
        .unwrap()
    }

    /// Sends `payload` to `target` and waits for the echo, retrying while the association is
    /// not up (it is being rebuilt).
    async fn round_trip(
        assoc: &Socks5UdpAssociation,
        target: TargetAddr<'_>,
        payload: &[u8],
    ) -> SocketAddr {
        tokio::time::timeout(WAIT, async {
            loop {
                if assoc.send_to(payload, target).await.is_err() {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
                let mut buf = [0u8; 2048];
                match tokio::time::timeout(Duration::from_millis(500), assoc.recv_from(&mut buf))
                    .await
                {
                    Ok(Ok((len, from))) => {
                        assert_eq!(&buf[..len], payload);
                        return from;
                    }
                    Ok(Err(e)) => panic!("recv_from failed: {e}"),
                    Err(_) => continue,
                }
            }
        })
        .await
        .expect("no echo through the proxy")
    }

    #[tokio::test]
    async fn datagrams_go_through_the_relay() {
        let proxy = FakeProxy::start(FakeProxyOptions::new(FakeProxyKind::Socks5)).await;
        let echo = echo().await;
        let assoc = association(&proxy);
        let from = round_trip(&assoc, echo.addr.into(), b"hello").await;
        assert_eq!(from, echo.addr);
        assert!(assoc.is_up());

        let relayed = proxy.datagrams();
        assert_eq!(relayed.len(), 1);
        assert_eq!(relayed[0].target, echo.addr.to_string());
        assert_eq!(relayed[0].payload, b"hello");
        // The destination only saw the relay.
        assert_eq!(*echo.sources.lock(), vec![relayed[0].from]);
        assert_eq!(proxy.associations(), 1);
    }

    #[tokio::test]
    async fn credentials_are_sent() {
        let mut opts = FakeProxyOptions::new(FakeProxyKind::Socks5);
        opts.credentials = Some(("us:er".to_owned(), "p@ss".to_owned()));
        let proxy = FakeProxy::start(opts).await;
        let echo = echo().await;
        let assoc = association(&proxy);
        round_trip(&assoc, echo.addr.into(), b"x").await;
        assert_eq!(proxy.auth_failures(), 0);
    }

    #[tokio::test]
    async fn wrong_password_is_an_error_and_nothing_is_sent() {
        let mut opts = FakeProxyOptions::new(FakeProxyKind::Socks5);
        opts.credentials = Some(("user".to_owned(), "right".to_owned()));
        let proxy = FakeProxy::start(opts).await;
        let echo = echo().await;
        let assoc = Socks5UdpAssociation::new(
            ProxyConfig::parse(&proxy.url_with_credentials("user", "wrong")).unwrap(),
            &CancellationToken::new(),
        )
        .unwrap();
        let err = assoc.send_to(b"x", echo.addr.into()).await.unwrap_err();
        let text = err.to_string();
        assert!(matches!(err, ProxyError::UdpAssociationDown(_)), "{text}");
        assert!(
            text.contains("rejected the user name or password"),
            "{text}"
        );
        assert!(!text.contains("wrong"), "{text}");
        assert!(echo.sources.lock().is_empty());
        assert_eq!(proxy.auth_failures(), 1);
    }

    #[tokio::test]
    async fn unspecified_relay_address_means_the_proxy_host() {
        let mut opts = FakeProxyOptions::new(FakeProxyKind::Socks5);
        opts.unspecified_relay_addr = true;
        let proxy = FakeProxy::start(opts).await;
        let echo = echo().await;
        let assoc = association(&proxy);
        assert_eq!(round_trip(&assoc, echo.addr.into(), b"x").await, echo.addr);
    }

    #[tokio::test]
    async fn host_names_are_sent_to_the_proxy() {
        let mut opts = FakeProxyOptions::new(FakeProxyKind::Socks5);
        opts.remote_dns = true;
        let proxy = FakeProxy::start(opts).await;
        let echo = echo().await;
        let assoc = association(&proxy);
        let from = round_trip(
            &assoc,
            TargetAddr::Domain("localhost", echo.addr.port()),
            b"x",
        )
        .await;
        assert_eq!(from.port(), echo.addr.port());
        let relayed = proxy.datagrams();
        assert_eq!(relayed[0].target, format!("localhost:{}", echo.addr.port()));
    }

    #[tokio::test]
    async fn a_dropped_control_connection_is_rebuilt() {
        let proxy = FakeProxy::start(FakeProxyOptions::new(FakeProxyKind::Socks5)).await;
        let echo = echo().await;
        let assoc = association(&proxy);
        round_trip(&assoc, echo.addr.into(), b"one").await;
        let first_relay = proxy.datagrams()[0].from;

        proxy.drop_udp_associations();
        tokio::time::timeout(WAIT, async {
            while proxy.associations() < 2 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the association was not rebuilt");
        round_trip(&assoc, echo.addr.into(), b"two").await;
        let last = proxy.datagrams().pop().unwrap();
        assert_eq!(last.payload, b"two");
        assert_ne!(last.from, first_relay, "the old relay was used");
    }

    #[tokio::test]
    async fn proxy_down_fails_and_recovers() {
        let proxy = FakeProxy::start(FakeProxyOptions::new(FakeProxyKind::Socks5)).await;
        let echo = echo().await;
        let assoc = association(&proxy);
        round_trip(&assoc, echo.addr.into(), b"before").await;

        proxy.stop().await;
        tokio::time::timeout(WAIT, async {
            while assoc.is_up() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the association did not notice the proxy is down");
        let sent_before = echo.sources.lock().len();
        let err = assoc.send_to(b"down", echo.addr.into()).await.unwrap_err();
        assert!(matches!(err, ProxyError::UdpAssociationDown(_)), "{err}");
        assert_eq!(
            echo.sources.lock().len(),
            sent_before,
            "something got through"
        );

        proxy.restart().await;
        round_trip(&assoc, echo.addr.into(), b"after").await;
        assert_eq!(proxy.associations(), 2);
        // Everything the destination got came from the proxy's relays.
        let relays: Vec<SocketAddr> = proxy.datagrams().iter().map(|d| d.from).collect();
        assert!(echo.sources.lock().iter().all(|s| relays.contains(s)));
    }

    #[tokio::test]
    async fn a_proxy_without_udp_is_an_error() {
        let mut opts = FakeProxyOptions::new(FakeProxyKind::Socks5);
        opts.udp = false;
        let proxy = FakeProxy::start(opts).await;
        let assoc = association(&proxy);
        let err = assoc
            .send_to(b"x", SocketAddr::from((Ipv4Addr::LOCALHOST, 9)).into())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("command not supported"), "{err}");
    }

    #[tokio::test]
    async fn only_socks5_relays_udp() {
        for url in [
            "socks4://127.0.0.1:1",
            "socks4a://127.0.0.1:1",
            "http://127.0.0.1:1",
        ] {
            let proxy = ProxyConfig::parse(url).unwrap();
            assert!(matches!(
                Socks5UdpAssociation::new(proxy, &CancellationToken::new()),
                Err(ProxyError::UdpNotSupported(_))
            ));
        }
    }

    #[tokio::test]
    async fn size_limits_count_the_header() {
        let proxy = FakeProxy::start(FakeProxyOptions::new(FakeProxyKind::Socks5)).await;
        let echo = echo().await;
        let assoc = association(&proxy);
        round_trip(&assoc, echo.addr.into(), b"x").await;
        let fits = vec![0u8; MAX_UDP_PAYLOAD - SOCKS5_UDP_HEADER_MIN];
        let too_large = vec![0u8; fits.len() + 1];
        assert!(matches!(
            assoc.send_to(&too_large, echo.addr.into()).await,
            Err(ProxyError::DatagramTooLarge)
        ));
        let name = "a".repeat(256);
        assert!(matches!(
            assoc.send_to(b"x", TargetAddr::Domain(&name, 1)).await,
            Err(ProxyError::DomainTooLong)
        ));
        let mut buf = Vec::new();
        write_udp_header(&mut buf, TargetAddr::Domain(&"a".repeat(255), 1)).unwrap();
        assert_eq!(buf.len(), SOCKS5_UDP_HEADER_MAX);
    }

    #[tokio::test]
    async fn shuts_down_with_its_token() {
        let proxy = FakeProxy::start(FakeProxyOptions::new(FakeProxyKind::Socks5)).await;
        let echo = echo().await;
        let token = CancellationToken::new();
        let assoc =
            Socks5UdpAssociation::new(ProxyConfig::parse(&proxy.url()).unwrap(), &token).unwrap();
        round_trip(&assoc, echo.addr.into(), b"x").await;
        token.cancel();
        let mut buf = [0u8; 64];
        let r = tokio::time::timeout(WAIT, assoc.recv_from(&mut buf))
            .await
            .unwrap();
        assert!(matches!(r, Err(ProxyError::UdpAssociationShutDown)));
    }

    #[test]
    fn headers() {
        let mut buf = Vec::new();
        write_udp_header(&mut buf, SocketAddr::from(([1, 2, 3, 4], 0x1234)).into()).unwrap();
        assert_eq!(buf, [0, 0, 0, 1, 1, 2, 3, 4, 0x12, 0x34]);
        assert_eq!(
            parse_udp_header(&buf),
            Ok((10, Some(SocketAddr::from(([1, 2, 3, 4], 0x1234)))))
        );

        buf.clear();
        let mapped: SocketAddr = "[::ffff:1.2.3.4]:80".parse().unwrap();
        write_udp_header(&mut buf, mapped.into()).unwrap();
        assert_eq!(buf, [0, 0, 0, 1, 1, 2, 3, 4, 0, 80]);

        buf.clear();
        let v6: SocketAddr = "[2001:db8::1]:80".parse().unwrap();
        write_udp_header(&mut buf, v6.into()).unwrap();
        assert_eq!(buf.len(), 22);
        assert_eq!(parse_udp_header(&buf), Ok((22, Some(v6))));

        buf.clear();
        write_udp_header(&mut buf, TargetAddr::Domain("ab", 80)).unwrap();
        assert_eq!(buf, [0, 0, 0, 3, 2, b'a', b'b', 0, 80]);
        assert_eq!(
            parse_udp_header(&buf),
            Ok((9, None)),
            "a host name as the source"
        );
        for len in 0..9 {
            assert!(parse_udp_header(&buf[..len]).is_err());
        }
        assert!(
            parse_udp_header(&[0, 0, 0, 3, 0, 0, 80]).is_err(),
            "an empty name"
        );

        assert!(
            parse_udp_header(&[0, 0, 1, 1, 1, 2, 3, 4, 0, 80]).is_err(),
            "FRAG"
        );
        assert!(
            parse_udp_header(&[0, 0, 0, 9, 1, 2, 3, 4, 0, 80]).is_err(),
            "ATYP"
        );
        for len in 0..10 {
            assert!(parse_udp_header(&[0, 0, 0, 1, 1, 2, 3, 4, 0, 80][..len]).is_err());
        }
        for len in 0..22 {
            assert!(
                parse_udp_header(&[[0, 0, 0, 4].as_slice(), &[0; 18]].concat()[..len]).is_err()
            );
        }
    }

    /// A hand-written SOCKS5 server whose relay is a socket of the test, to send the client
    /// datagrams it must drop.
    #[tokio::test]
    async fn bad_datagrams_from_the_relay_are_dropped() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let relay = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let relay_port = relay.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut control, _) = listener.accept().await.unwrap();
            let mut greeting = [0u8; 3];
            control.read_exact(&mut greeting).await.unwrap();
            control.write_all(&[5, 0]).await.unwrap();
            let mut request = [0u8; 10];
            control.read_exact(&mut request).await.unwrap();
            assert_eq!(request[..2], [5, 3]);
            let mut reply = vec![5, 0, 0, 1, 127, 0, 0, 1];
            reply.extend_from_slice(&relay_port.to_be_bytes());
            control.write_all(&reply).await.unwrap();
            // Keep the association alive.
            let mut rest = Vec::new();
            let _ = control.read_to_end(&mut rest).await;
        });

        let assoc = Socks5UdpAssociation::new(
            ProxyConfig::parse(&format!("socks5://{proxy_addr}")).unwrap(),
            &CancellationToken::new(),
        )
        .unwrap();
        let target = SocketAddr::from((Ipv4Addr::new(10, 0, 0, 1), 6881));
        assoc.send_to(b"ping", target.into()).await.unwrap();
        let mut buf = [0u8; 1024];
        let (len, client) = relay.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..len], b"\0\0\0\x01\x0a\0\0\x01\x1a\xe1ping");

        let from_target = |frag: u8, payload: &[u8]| {
            let mut d = vec![0, 0, frag, 1, 10, 0, 0, 1, 0x1a, 0xe1];
            d.extend_from_slice(payload);
            d
        };
        // Fragmented, a host name as the source, truncated, and from another socket.
        relay
            .send_to(&from_target(1, b"frag"), client)
            .await
            .unwrap();
        relay
            .send_to(b"\0\0\0\x03\x01a\x1a\xe1name", client)
            .await
            .unwrap();
        relay.send_to(b"\0\0\0\x01\x0a", client).await.unwrap();
        let stranger = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        stranger
            .send_to(&from_target(0, b"stranger"), client)
            .await
            .unwrap();
        relay
            .send_to(&from_target(0, b"pong"), client)
            .await
            .unwrap();

        let (len, from) = tokio::time::timeout(WAIT, assoc.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[..len], b"pong");
        assert_eq!(from, target);
        drop(assoc);
        tokio::time::timeout(WAIT, server).await.unwrap().unwrap();
    }

    /// A hand-written SOCKS5 server that accepts any number of control connections, answers
    /// UDP ASSOCIATE with `BND.ADDR` = `bound` (ATYP and address bytes) and the port of
    /// `relay`, a socket of the test, and keeps the control connections open.
    struct ScriptedProxy {
        addr: SocketAddr,
        relay: UdpSocket,
        accepted: Arc<AtomicU32>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for ScriptedProxy {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl ScriptedProxy {
        async fn start(bound: &[u8]) -> Self {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let addr = listener.local_addr().unwrap();
            let relay = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let mut reply = vec![5, 0, 0];
            reply.extend_from_slice(bound);
            reply.extend_from_slice(&relay.local_addr().unwrap().port().to_be_bytes());
            let accepted = Arc::new(AtomicU32::new(0));
            let task = tokio::spawn({
                let accepted = accepted.clone();
                async move {
                    while let Ok((mut control, _)) = listener.accept().await {
                        accepted.fetch_add(1, Ordering::Relaxed);
                        let reply = reply.clone();
                        tokio::spawn(async move {
                            let mut greeting = [0u8; 3];
                            control.read_exact(&mut greeting).await.unwrap();
                            control.write_all(&[5, 0]).await.unwrap();
                            let mut request = [0u8; 10];
                            control.read_exact(&mut request).await.unwrap();
                            assert_eq!(request[..2], [5, 3]);
                            control.write_all(&reply).await.unwrap();
                            let mut rest = Vec::new();
                            let _ = control.read_to_end(&mut rest).await;
                        });
                    }
                }
            });
            Self {
                addr,
                relay,
                accepted,
                task,
            }
        }

        fn association(&self) -> Socks5UdpAssociation {
            Socks5UdpAssociation::new(
                ProxyConfig::parse(&format!("socks5://{}", self.addr)).unwrap(),
                &CancellationToken::new(),
            )
            .unwrap()
        }

        fn accepted(&self) -> u32 {
            self.accepted.load(Ordering::Relaxed)
        }
    }

    #[tokio::test]
    async fn host_name_sources_are_delivered_by_recv() {
        let proxy = ScriptedProxy::start(&[1, 127, 0, 0, 1]).await;
        let assoc = proxy.association();
        assoc
            .send_to(b"ping", TargetAddr::Domain("tracker.example", 80))
            .await
            .unwrap();
        let mut buf = [0u8; 1024];
        let (_, client) = proxy.relay.recv_from(&mut buf).await.unwrap();
        let mut answer = b"\0\0\0\x03\x0ftracker.example\0\x50".to_vec();
        answer.extend_from_slice(b"pong");
        proxy.relay.send_to(&answer, client).await.unwrap();
        let (len, source) = tokio::time::timeout(WAIT, assoc.recv(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[..len], b"pong");
        assert_eq!(source, None);
    }

    #[tokio::test]
    async fn a_relay_given_by_name_is_the_proxy_host() {
        // A name nobody can resolve: it must not be looked up at all.
        let proxy = ScriptedProxy::start(b"\x03\x0drelay.invalid").await;
        let assoc = proxy.association();
        assoc
            .send_to(b"x", SocketAddr::from((Ipv4Addr::LOCALHOST, 9)).into())
            .await
            .unwrap();
        let mut buf = [0u8; 64];
        let (len, _) = tokio::time::timeout(WAIT, proxy.relay.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(buf.get(len - 1), Some(&b'x'));
    }

    /// A relay that swallows everything while its control connection stays open (a proxy host
    /// that vanished without closing the connection) is given up and a new association made.
    #[tokio::test]
    async fn a_silent_relay_is_rebuilt() {
        let proxy = ScriptedProxy::start(&[1, 127, 0, 0, 1]).await;
        let assoc = proxy.association();
        let target = TargetAddr::from(SocketAddr::from((Ipv4Addr::LOCALHOST, 9)));
        tokio::time::timeout(WAIT, async {
            while proxy.accepted() < 2 {
                // Fails while the association is being rebuilt.
                let _ = assoc.send_to(b"x", target).await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the silent association was kept");
    }

    /// An answer, however rare, keeps the association: only silence counts.
    #[tokio::test]
    async fn a_relay_that_answers_is_kept() {
        let proxy = ScriptedProxy::start(&[1, 127, 0, 0, 1]).await;
        let assoc = proxy.association();
        let target = SocketAddr::from((Ipv4Addr::LOCALHOST, 9));
        let mut buf = [0u8; 64];
        let started = Instant::now();
        while started.elapsed() < SILENT_TIMEOUT * 3 {
            for _ in 0..SILENT_SENDS {
                assoc.send_to(b"x", target.into()).await.unwrap();
            }
            let mut client = None;
            for _ in 0..SILENT_SENDS {
                client = Some(proxy.relay.recv_from(&mut buf).await.unwrap().1);
            }
            let mut answer = vec![0, 0, 0, 1, 127, 0, 0, 1, 0, 9];
            answer.push(b'y');
            proxy.relay.send_to(&answer, client.unwrap()).await.unwrap();
            tokio::time::timeout(WAIT, assoc.recv_from(&mut buf))
                .await
                .unwrap()
                .unwrap();
            tokio::time::sleep(SILENT_TIMEOUT / 4).await;
        }
        assert_eq!(proxy.accepted(), 1);
    }

    /// A proxy that refuses UDP ASSOCIATE is not asked again soon.
    #[tokio::test]
    async fn a_refusal_is_not_retried_soon() {
        let mut opts = FakeProxyOptions::new(FakeProxyKind::Socks5);
        opts.udp = false;
        let proxy = FakeProxy::start(opts).await;
        let assoc = association(&proxy);
        let target = TargetAddr::from(SocketAddr::from((Ipv4Addr::LOCALHOST, 9)));
        let started = Instant::now();
        // The normal backoff would have made three more attempts by then.
        while started.elapsed() < MIN_BACKOFF * 7 {
            assert!(assoc.send_to(b"x", target).await.is_err());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(proxy.accepted(), 1);
    }

    #[test]
    fn internal_relay_addresses_mean_the_proxy_host() {
        let addr = |s: &str| -> SocketAddr { s.parse().unwrap() };
        let ip = |s: &str| -> IpAddr { s.parse().unwrap() };
        for (bound, proxy, want) in [
            // Unspecified: always the proxy host.
            ("0.0.0.0:7000", "203.0.113.5", "203.0.113.5:7000"),
            ("[::]:7000", "2001:db8::5", "[2001:db8::5]:7000"),
            // Loopback or private while the proxy is reached elsewhere.
            ("127.0.0.1:7000", "203.0.113.5", "203.0.113.5:7000"),
            ("172.17.0.2:7000", "203.0.113.5", "203.0.113.5:7000"),
            ("100.64.1.1:7000", "203.0.113.5", "203.0.113.5:7000"),
            ("[fd00::2]:7000", "2001:db8::5", "[2001:db8::5]:7000"),
            // Taken as given: a relay on another public host, a private relay of a proxy on
            // the local network, a loopback relay of a local proxy.
            ("198.51.100.7:7000", "203.0.113.5", "198.51.100.7:7000"),
            ("192.168.1.3:7000", "192.168.1.2", "192.168.1.3:7000"),
            ("127.0.0.1:7000", "127.0.0.1", "127.0.0.1:7000"),
            ("10.0.0.3:7000", "127.0.0.1", "10.0.0.3:7000"),
            ("[2001:db8::7]:7000", "2001:db8::5", "[2001:db8::7]:7000"),
        ] {
            assert_eq!(
                relay_address(addr(bound), ip(proxy)),
                addr(want),
                "{bound} from {proxy}"
            );
        }
    }
}
