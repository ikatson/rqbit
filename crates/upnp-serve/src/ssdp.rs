use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
    time::Duration,
};

use anyhow::{bail, Context};
use bstr::BStr;
use network_interface::NetworkInterfaceConfig;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

use crate::constants::{UPNP_DEVICE_MEDIASERVER, UPNP_DEVICE_ROOT};

const SSDP_PORT: u16 = 1900;
const SSDM_MCAST_IPV4: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const SSDP_MCAST_IPV6_LINK_LOCAL: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xc);
const SSDP_MCAST_IPV6_SITE_LOCAL: Ipv6Addr = Ipv6Addr::new(0xff05, 0, 0, 0, 0, 0, 0, 0xc);

const NTS_ALIVE: &str = "ssdp:alive";
const NTS_BYEBYE: &str = "ssdp:byebye";

fn ipv6_is_link_local(ip: Ipv6Addr) -> bool {
    const LL: Ipv6Addr = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0);
    const MASK: Ipv6Addr = Ipv6Addr::new(0xffff, 0xffff, 0xffff, 0xffff, 0, 0, 0, 0);

    ip.to_bits() & MASK.to_bits() == LL.to_bits() & MASK.to_bits()
}

#[derive(Debug)]
pub enum SsdpMessage<'a, 'h> {
    MSearch(SsdpMSearchRequest<'a>),
    #[allow(dead_code)]
    OtherRequest(httparse::Request<'h, 'a>),
    #[allow(dead_code)]
    Response(httparse::Response<'h, 'a>),
}

#[derive(Debug)]
pub struct SsdpMSearchRequest<'a> {
    #[allow(dead_code)]
    pub host: &'a BStr,
    pub man: &'a BStr,
    pub st: &'a BStr,
}

impl<'a> SsdpMSearchRequest<'a> {
    fn matches_media_server(&self) -> bool {
        if self.man != "\"ssdp:discover\"" {
            return false;
        }
        if self.st == UPNP_DEVICE_ROOT || self.st == UPNP_DEVICE_MEDIASERVER {
            return true;
        }
        false
    }
}

pub fn try_parse_ssdp<'a, 'h>(
    buf: &'a [u8],
    headers: &'h mut [httparse::Header<'a>],
) -> anyhow::Result<SsdpMessage<'a, 'h>> {
    if buf.starts_with(b"HTTP/") {
        let mut resp = httparse::Response::new(headers);
        resp.parse(buf).context("error parsing response")?;
        return Ok(SsdpMessage::Response(resp));
    }

    let mut req = httparse::Request::new(headers);
    req.parse(buf).context("error parsing request")?;

    match req.method {
        Some("M-SEARCH") => {
            let mut host = None;
            let mut man = None;
            let mut st = None;

            for header in req.headers.iter() {
                match header.name {
                    "HOST" | "Host" | "host" => host = Some(header.value),
                    "MAN" | "Man" | "man" => man = Some(header.value),
                    "ST" | "St" | "st" => st = Some(header.value),
                    other => trace!(header=?BStr::new(other), "ignoring SSDP header"),
                }
            }

            match (host, man, st) {
                (Some(host), Some(man), Some(st)) => {
                    return Ok(SsdpMessage::MSearch(SsdpMSearchRequest {
                        host: BStr::new(host),
                        man: BStr::new(man),
                        st: BStr::new(st),
                    }))
                }
                _ => bail!("not all of host, man and st are set"),
            }
        }
        _ => return Ok(SsdpMessage::OtherRequest(req)),
    }
}

pub struct SsdpRunnerOptions {
    pub usn: String,
    pub description_http_location: url::Url,
    pub server_string: String,
    pub notify_interval: Duration,
    pub shutdown: CancellationToken,
}

struct UdpSocket {
    sock2: socket2::Socket,
    tokio: tokio::net::UdpSocket,
}

pub struct SsdpRunner {
    opts: SsdpRunnerOptions,
    socket_v4: Option<UdpSocket>,
    socket_v6: Option<UdpSocket>,
}

fn socket_presetup(bind_addr: SocketAddr) -> anyhow::Result<UdpSocket> {
    let domain = if bind_addr.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    let sock = socket2::Socket::new(domain, socket2::Type::DGRAM, None)
        .context(bind_addr)
        .context("error creating socket")?;
    #[cfg(not(target_os = "windows"))]
    sock.set_reuse_port(true)
        .context("error setting SO_REUSEPORT")?;
    sock.set_reuse_address(true)
        .context("error setting SO_REUSEADDR")?;

    trace!(addr=?bind_addr, "binding UDP");
    sock.bind(&bind_addr.into())
        .context(bind_addr)
        .context("error binding")?;

    sock.set_nonblocking(true)?;

    let sock_clone = sock.try_clone().context("can't clone socket")?;

    let tokio_socket = tokio::net::UdpSocket::from_std(sock_clone.into())
        .context("error converting socket2 socket to tokio")?;

    Ok(UdpSocket {
        sock2: sock,
        tokio: tokio_socket,
    })
}

async fn bind_v4_socket() -> anyhow::Result<UdpSocket> {
    let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, SSDP_PORT);
    let socket = socket_presetup(bind_addr.into())?;

    let default_multiast_membership_ip = std::iter::once(Ipv4Addr::UNSPECIFIED);
    let all_multicast_membership_ips = network_interface::NetworkInterface::show()
        .into_iter()
        .flatten()
        .flat_map(|nic| nic.addr.into_iter())
        .filter_map(|addr| {
            let ip = addr.ip();
            match ip {
                std::net::IpAddr::V4(addr) if addr.is_private() && !addr.is_loopback() => {
                    Some(addr)
                }
                _ => None,
            }
        });

    for ifaddr in default_multiast_membership_ip.chain(all_multicast_membership_ips) {
        trace!(multiaddr=?SSDM_MCAST_IPV4, interface=?ifaddr, "joining multicast v4 group");
        if let Err(e) = socket.tokio.join_multicast_v4(SSDM_MCAST_IPV4, ifaddr) {
            debug!(multiaddr=?SSDM_MCAST_IPV4, interface=?ifaddr, "error joining multicast v4 group: {e:#}");
        }
    }

    Ok(socket)
}

async fn bind_v6_socket() -> anyhow::Result<UdpSocket> {
    let bind_addr = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, SSDP_PORT, 0, 0);
    let socket = socket_presetup(bind_addr.into())?;

    for nic in network_interface::NetworkInterface::show()
        .into_iter()
        .flatten()
    {
        let mut has_link_local = false;
        let mut has_site_local = false;
        for addr in nic.addr.iter() {
            let addr = match addr.ip() {
                IpAddr::V4(_) => continue,
                IpAddr::V6(v6) => v6,
            };
            if addr.is_loopback() {
                continue;
            }
            if ipv6_is_link_local(addr) {
                has_link_local = true;
            } else {
                has_site_local = true;
            }
        }
        for (present, multiaddr) in [
            (has_site_local, SSDP_MCAST_IPV6_SITE_LOCAL),
            (has_link_local, SSDP_MCAST_IPV6_LINK_LOCAL),
        ] {
            if !present {
                continue;
            }
            trace!(multiaddr=?multiaddr, interface=?nic.index, "joining multicast v6 group");
            if let Err(e) = socket.tokio.join_multicast_v6(&multiaddr, nic.index) {
                debug!(multiaddr=?multiaddr, interface=?nic.index, "error joining multicast v6 group: {e:#}");
            }
        }
    }

    Ok(socket)
}

struct MulticastOpts {
    interface_addr: IpAddr,
    #[allow(dead_code)]
    interface_id: u32,
    mcast_addr: SocketAddr,
}

impl MulticastOpts {
    fn addr_no_scope(&self) -> SocketAddr {
        let mut addr = self.mcast_addr;
        if let SocketAddr::V6(v6) = &mut addr {
            v6.set_scope_id(0);
        }
        addr
    }
}

impl SsdpRunner {
    pub async fn new(opts: SsdpRunnerOptions) -> anyhow::Result<Self> {
        let socket_v4 = bind_v4_socket()
            .await
            .map_err(|e| warn!("error creating IPv4 SSDP socket: {e:#}"))
            .ok();
        let socket_v6 = bind_v6_socket()
            .await
            .map_err(|e| warn!("error creating IPv6 SSDP socket: {e:#}"))
            .ok();
        Ok(Self {
            opts,
            socket_v4,
            socket_v6,
        })
    }

    fn generate_notify_message(
        &self,
        device_kind: &str,
        nts: &str,
        opts: &MulticastOpts,
    ) -> String {
        let usn: &str = &self.opts.usn;
        let server: &str = &self.opts.server_string;
        let host = opts.addr_no_scope();
        let mut location = self.opts.description_http_location.clone();
        let _ = location.set_ip_host(opts.interface_addr);
        format!(
            "NOTIFY * HTTP/1.1\r
Host: {host}\r
Cache-Control: max-age=75\r
Location: {location}\r
NT: {device_kind}\r
NTS: {nts}\r
Server: {server}\r
USN: {usn}::{device_kind}\r
\r
"
        )
    }

    fn generate_ssdp_discover_response(
        &self,
        st: &str,
        addr: SocketAddr,
    ) -> anyhow::Result<String> {
        let local_ip = ::librqbit_upnp::get_local_ip_relative_to(addr.ip())?;
        let location = {
            let mut loc = self.opts.description_http_location.clone();
            let _ = loc.set_ip_host(local_ip);
            loc
        };
        let usn = &self.opts.usn;
        let server = &self.opts.server_string;
        Ok(format!(
            "HTTP/1.1 200 OK\r
Cache-Control: max-age=75\r
Ext: \r
Location: {location}\r
Server: {server}\r
St: {st}\r
Usn: {usn}::{st}\r
Content-Length: 0\r\n\r\n"
        ))
    }

    async fn try_send_mcast_everywhere(
        &self,
        get_payload: &impl Fn(&MulticastOpts) -> bstr::BString,
    ) {
        use network_interface::NetworkInterfaceConfig;
        let interfaces = network_interface::NetworkInterface::show();
        let interfaces = match interfaces {
            Ok(interfaces) => interfaces,
            Err(e) => {
                warn!(error=?e, "error determining network interfaces");
                return;
            }
        };

        let sent = Mutex::new(HashSet::new());
        let sent = &sent;

        let futs = interfaces
            .into_iter()
            .flat_map(|ni|
                ni.addr.into_iter().map(move |a| (ni.index, a))
            )
            .filter_map(|(ifidx, addr)| match addr.ip() {
                std::net::IpAddr::V4(a) if !a.is_loopback() && a.is_private() => {
                    Some(MulticastOpts {
                        interface_addr: addr.ip(),
                        interface_id: ifidx,
                        mcast_addr: SocketAddr::V4(SocketAddrV4::new(SSDM_MCAST_IPV4, SSDP_PORT)),
                    })
                }
                std::net::IpAddr::V6(a) if !a.is_loopback() => Some(MulticastOpts {
                    interface_addr: addr.ip(),
                    interface_id: ifidx,
                    mcast_addr: {
                        if ipv6_is_link_local(a) {
                            SocketAddr::V6(SocketAddrV6::new(SSDP_MCAST_IPV6_LINK_LOCAL, SSDP_PORT, 0, ifidx))
                        } else {
                            SocketAddr::V6(SocketAddrV6::new(SSDP_MCAST_IPV6_SITE_LOCAL, SSDP_PORT, 0, ifidx))
                        }
                    },
                }),
                _ => {
                    trace!(oif_id=ifidx, addr=%addr.ip(), "ignoring address");
                    None
                },
            })
            .map(|opts| async move {
                let payload = get_payload(&opts);
                if !sent
                    .lock()
                    .insert((payload.clone(), opts.interface_id, opts.mcast_addr))
                {
                    trace!(oif_id=opts.interface_id, addr=%opts.mcast_addr, "not sending duplicate payload");
                    return;
                }

                let sock = match (
                    opts.interface_addr,
                    self.socket_v4.as_ref(),
                    self.socket_v6.as_ref(),
                ) {
                    // Call setsockopt(IP_MULTICAST_IF), so that the message
                    // gets sent out of the interface we want (otherwise it'll get sent through
                    // default one).
                    (IpAddr::V4(ip), Some(sock_v4), _) => {
                        if let Err(e) = sock_v4.sock2.set_multicast_if_v4(&ip) {
                            debug!(addr=%ip, "error calling set_multicast_if_v4: {e:#}");
                        }
                        sock_v4
                    }
                    (IpAddr::V6(_), _, Some(sock_v6)) => {
                        if let Err(e) = sock_v6.sock2.set_multicast_if_v6(opts.interface_id) {
                            debug!(oif_id=opts.interface_id, "error calling set_multicast_if_v6: {e:#}");
                        }
                        sock_v6
                    },
                    _ => {
                        trace!(addr=%opts.interface_addr, "ignoring address, no socket to send to");
                        return;
                    },
                };

                match sock.tokio.send_to(payload.as_slice(), opts.mcast_addr).await {
                    Ok(sz) => trace!(addr=%opts.mcast_addr, oif_id=opts.interface_id, oif_addr=%opts.interface_addr, size=sz, payload=?payload, "sent"),
                    Err(e) => {
                        debug!(addr=%opts.mcast_addr, oif_id=opts.interface_id, oif_addr=%opts.interface_addr, payload=?payload, "error sending: {e:#}")
                    }
                };
            });

        futures::future::join_all(futs).await;
    }

    async fn try_send_notifies(&self, nts: &str) {
        self.try_send_mcast_everywhere(&|opts| {
            self.generate_notify_message(UPNP_DEVICE_MEDIASERVER, nts, opts)
                .into()
        })
        .await
    }

    async fn task_send_alive_notifies_periodically(&self) {
        let mut interval = tokio::time::interval(self.opts.notify_interval);
        loop {
            interval.tick().await;
            self.try_send_notifies(NTS_ALIVE).await;
        }
    }

    async fn process_incoming_message(
        &self,
        msg: &[u8],
        sock: &UdpSocket,
        addr: SocketAddr,
    ) -> anyhow::Result<()> {
        let mut headers = [httparse::EMPTY_HEADER; 16];
        trace!(content = ?BStr::new(msg), ?addr, "received message");
        let parsed = try_parse_ssdp(msg, &mut headers);
        let msg = match parsed {
            Ok(SsdpMessage::MSearch(msg)) => msg,
            Ok(m) => {
                trace!("ignoring {m:?}");
                return Ok(());
            }
            Err(e) => {
                debug!(error=?e, "error parsing SSDP message");
                return Ok(());
            }
        };
        if !msg.matches_media_server() {
            trace!("not a media server request, ignoring");
            return Ok(());
        }

        if let Ok(st) = std::str::from_utf8(msg.st) {
            let response = self.generate_ssdp_discover_response(st, addr)?;
            trace!(content = response, ?addr, "sending SSDP discover response");
            sock.tokio
                .send_to(response.as_bytes(), addr)
                .await
                .context("error sending")?;
        }

        Ok(())
    }

    async fn task_respond_on_msearches(&self, sock: Option<&UdpSocket>) {
        let mut buf = vec![0u8; 16184];
        let sock = match sock {
            Some(sock) => sock,
            None => return,
        };

        loop {
            let (sz, addr) = match sock.tokio.recv_from(&mut buf).await {
                Ok((sz, addr)) => (sz, addr),
                Err(e) => {
                    warn!(error=?e, "error receving");
                    return;
                }
            };
            let msg = &buf[..sz];
            if let Err(e) = self.process_incoming_message(msg, sock, addr).await {
                warn!(error=?e, ?addr, "error processing incoming SSDP message")
            }
        }
    }

    async fn try_send_example_msearch(&self) {
        self.try_send_mcast_everywhere(&|opts| {
            let dest = opts.addr_no_scope();
            format!(
                "M-SEARCH * HTTP/1.1\r
HOST: {dest}\r
ST: urn:schemas-upnp-org:device:MediaServer:1\r
MAN: \"ssdp:discover\"\r
MX: 2\r\n\r\n"
            )
            .into()
        })
        .await
    }

    pub async fn run_forever(&self) -> anyhow::Result<()> {
        // This isn't necessary, but would show that it works.
        let t0 = self.try_send_example_msearch();
        let t1 = self.task_respond_on_msearches(self.socket_v4.as_ref());
        let t2 = self.task_respond_on_msearches(self.socket_v6.as_ref());
        let t3 = self.task_send_alive_notifies_periodically();

        let wait = async move {
            tokio::join!(t0, t1, t2, t3);
            Ok(())
        };

        tokio::select! {
            r = wait => r,
            _ = self.opts.shutdown.cancelled() => {
                self.try_send_notifies(NTS_BYEBYE).await;
                Ok(())
            }
        }
    }
}
