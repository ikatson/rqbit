use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    time::Duration,
};

use anyhow::{bail, Context};
use bstr::BStr;
use tokio::net::UdpSocket;
use tracing::{debug, trace, warn};

use crate::constants::{UPNP_KIND_MEDIASERVER, UPNP_KIND_ROOT_DEVICE};

const UPNP_PORT: u16 = 1900;
const UPNP_BROADCAST_IP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const UPNP_BROADCAST_ADDR: SocketAddrV4 = SocketAddrV4::new(UPNP_BROADCAST_IP, UPNP_PORT);

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
    pub host: &'a BStr,
    pub man: &'a BStr,
    pub st: &'a BStr,
}

impl<'a> SsdpMSearchRequest<'a> {
    fn matches_media_server(&self) -> bool {
        if self.host != "239.255.255.250:1900" {
            return false;
        }
        if self.man != "\"ssdp:discover\"" {
            return false;
        }
        if self.st == UPNP_KIND_ROOT_DEVICE || self.st == UPNP_KIND_MEDIASERVER {
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
    pub description_http_location: String,
    pub server_string: String,
    pub notify_interval: Duration,
}

pub struct SsdpRunner {
    opts: SsdpRunnerOptions,
    socket: UdpSocket,
}

impl SsdpRunner {
    pub async fn new(opts: SsdpRunnerOptions) -> anyhow::Result<Self> {
        let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, UPNP_PORT);
        trace!(addr=?bind_addr, "binding UDP");
        let socket =
            tokio::net::UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, UPNP_PORT))
                .await
                .context("error binding")?;

        trace!(multiaddr=?UPNP_BROADCAST_IP, interface=?Ipv4Addr::UNSPECIFIED, "joining multicast v4 group");
        socket
            .join_multicast_v4(UPNP_BROADCAST_IP, Ipv4Addr::UNSPECIFIED)
            .context("error joining multicast group")?;

        Ok(Self { opts, socket })
    }

    fn generate_notify_message(&self, kind: &str) -> String {
        let usn: &str = &self.opts.usn;
        let description_http_location = &self.opts.description_http_location;
        let server: &str = &self.opts.server_string;
        let bcast_addr = UPNP_BROADCAST_ADDR;
        format!(
            "NOTIFY * HTTP/1.1\r
Host: {bcast_addr}\r
Cache-Control: max-age=75\r
Location: {description_http_location}\r
NT: {kind}\r
NTS: ssdp:alive\r
Server: {server}\r
USN: {usn}::{kind}\r
\r
"
        )
    }

    fn generate_ssdp_discover_response(&self) -> String {
        let location = &self.opts.description_http_location;
        let usn = &self.opts.usn;
        let media_server = UPNP_KIND_MEDIASERVER;
        let server = &self.opts.server_string;
        format!(
            "HTTP/1.1 200 OK\r
Cache-Control: max-age=75\r
Ext: \r
Location: {location}\r
Server: {server}\r
St: {media_server}\r
Usn: {usn}::{media_server}\r
Content-Length: 0\r\n\r\n"
        )
    }

    async fn try_send_notifies(&self) {
        for kind in [UPNP_KIND_ROOT_DEVICE, UPNP_KIND_MEDIASERVER] {
            let msg = self.generate_notify_message(kind);
            trace!(content=?msg, addr=?UPNP_BROADCAST_ADDR, "sending SSDP NOTIFY");
            if let Err(e) = self
                .socket
                .send_to(msg.as_bytes(), UPNP_BROADCAST_ADDR)
                .await
            {
                warn!(error=?e, "error sending SSDP NOTIFY")
            }
        }
    }

    async fn task_send_notifies_periodically(&self) -> anyhow::Result<()> {
        let mut interval = tokio::time::interval(self.opts.notify_interval);
        loop {
            interval.tick().await;
            self.try_send_notifies().await;
        }
    }

    async fn process_incoming_message(&self, msg: &[u8], addr: SocketAddr) -> anyhow::Result<()> {
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

        let response = self.generate_ssdp_discover_response();
        trace!(content = response, ?addr, "sending SSDP discover response");
        self.socket
            .send_to(response.as_bytes(), addr)
            .await
            .context("error sending")?;

        Ok(())
    }

    async fn task_respond_on_msearches(&self) -> anyhow::Result<()> {
        let mut buf = vec![0u8; 16184];

        loop {
            let (sz, addr) = self
                .socket
                .recv_from(&mut buf)
                .await
                .context("error receiving")?;
            let msg = &buf[..sz];
            if let Err(e) = self.process_incoming_message(msg, addr).await {
                warn!(error=?e, ?addr, "error processing incoming SSDP message")
            }
        }
    }

    async fn send_msearch(&self) -> anyhow::Result<()> {
        let msearch_msg = "M-SEARCH * HTTP/1.1\r
HOST: 239.255.255.250:1900\r
ST: urn:schemas-upnp-org:device:MediaServer:1\r
MAN: \"ssdp:discover\"\r
MX: 2\r\n\r\n";

        trace!(content = msearch_msg, "multicasting M-SEARCH");

        self.socket
            .send_to(msearch_msg.as_bytes(), UPNP_BROADCAST_ADDR)
            .await
            .context("error sending msearch")?;
        Ok(())
    }

    pub async fn run_forever(&self) -> anyhow::Result<()> {
        // This isn't necessary, but would show that it works.
        self.send_msearch().await?;

        let t1 = self.task_respond_on_msearches();
        let t2 = self.task_send_notifies_periodically();

        tokio::pin!(t1);
        tokio::pin!(t2);

        tokio::select! {
            r = &mut t1 => r,
            r = &mut t2 => r,
        }
    }
}
