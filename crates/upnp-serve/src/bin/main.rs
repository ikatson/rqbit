use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::SystemTime;

use anyhow::bail;
use anyhow::Context;
use axum::body::Bytes;
use axum::extract::ConnectInfo;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use bstr::BStr;
use bstr::ByteSlice;
use httpdate::fmt_http_date;
use librqbit_buffers::ByteBuf;
use tokio::spawn;
use tower_http::trace::TraceLayer;
use tracing::debug;
use tracing::error;
use tracing::{info, warn};

#[derive(Debug)]
enum SsdpMessage<'a> {
    Notify(SsdpNotify),
    MSearch(SsdpMSearchRequest<'a>),
    Response(SsdpResponse<'a>),
}

#[derive(Debug)]
struct SsdpNotify {}

#[derive(Debug)]

struct SsdpResponse<'a> {
    raw: &'a BStr,
}

#[derive(Debug)]
struct SsdpMSearchRequest<'a> {
    host: &'a BStr,
    man: &'a BStr,
    st: &'a BStr,
}

const SSDP_SERVER_STRING: &str = "Linux/3.4 DLNADOC/1.50 UPnP/1.0 rqbit/1";

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

fn try_parse_ssdp(buf: &[u8]) -> anyhow::Result<SsdpMessage<'_>> {
    let mut host = None;
    let mut man = None;
    let mut st = None;

    let mut it = buf.split_str(b"\r\n").take_while(|l| !l.is_empty());
    match it.next() {
        Some(b"M-SEARCH * HTTP/1.1") => {
            for line in it {
                let line = BStr::new(line);
                let (k, v) = line
                    .split_once_str(": ")
                    .with_context(|| format!("invalid line, expected header. Line: {line:?}"))?;
                match k {
                    b"HOST" | b"Host" | b"host" => host = Some(v),
                    b"MAN" | b"Man" | b"man" => man = Some(v),
                    b"ST" | b"St" | b"st" => st = Some(v),
                    _ => debug!(header=?BStr::new(k), "ignoring SSDP header"),
                }
            }

            let msearch = match (host, man, st) {
                (Some(host), Some(man), Some(st)) => SsdpMSearchRequest {
                    host: BStr::new(host),
                    man: BStr::new(man),
                    st: BStr::new(st),
                },
                _ => bail!("not all of host, man and st are set"),
            };

            debug!(?msearch, "parsed");

            Ok(SsdpMessage::MSearch(msearch))
        }
        Some(b"NOTIFY * HTTP/1.1") => return Ok(SsdpMessage::Notify(SsdpNotify {})),
        Some(b"HTTP/1.1 200 OK") => {
            return Ok(SsdpMessage::Response(SsdpResponse {
                raw: BStr::new(buf),
            }))
        }
        _ => {
            bail!("not a known SSDP message, only M-SEARCH or NOTIFY supported")
        }
    }
}

struct MediaServerDescriptionSpec<'a> {
    friendly_name: &'a str,
    manufacturer: &'a str,
    model_name: &'a str,
    unique_id: &'a str,
    server_string: &'a str,
}

const HTTP_PORT: u16 = 9005;

const USN: &str = "uuid:9058e35c-9571-4754-8a37-00b6bf1a719d";

async fn generate_description(spec: &MediaServerDescriptionSpec<'_>) -> String {
    let friendly_name = spec.friendly_name;
    let manufacturer = spec.manufacturer;
    let model_name = spec.model_name;
    let unique_id = spec.unique_id;

    format!(
        r#"
            <?xml version="1.0"?>
            <root xmlns="urn:schemas-upnp-org:device-1-0">
                <specVersion>
                    <major>1</major>
                    <minor>0</minor>
                </specVersion>
                <device>
                    <deviceType>urn:schemas-upnp-org:device:MediaServer:1</deviceType>
                    <friendlyName>{friendly_name}</friendlyName>
                    <manufacturer>{manufacturer}</manufacturer>
                    <modelName>{model_name}</modelName>
                    <UDN>{unique_id}</UDN>

                    <serviceList>
                      <service>
                        <serviceType>urn:schemas-upnp-org:service:ContentDirectory:1</serviceType>
                        <serviceId>urn:upnp-org:serviceId:ContentDirectory</serviceId>
                        <SCPDURL>/scpd/ContentDirectory.xml</SCPDURL>
                        <controlURL>/control/ContentDirectory</controlURL>
                        <eventSubURL></eventSubURL>
                      </service>
                      <service>
                        <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
                        <serviceId>urn:upnp-org:serviceId:ConnectionManager</serviceId>
                        <SCPDURL>/scpd/ConnectionManager.xml</SCPDURL>
                        <controlURL>/control/ConnectionManager</controlURL>
                        <eventSubURL></eventSubURL>
                      </service>
                    </serviceList>
                    <presentationURL>/</presentationURL>
                </device>
            </root>

        "#
    )
}

const MEDIA_SERVER_DESCRIPTION: MediaServerDescriptionSpec<'static> = MediaServerDescriptionSpec {
    friendly_name: "Rust Friendly",
    manufacturer: "Igor K",
    model_name: "0.0.1",
    unique_id: USN,
    server_string: "Linux/3.4 DLNADOC/1.50 UPnP/1.0 dms/1",
};

async fn description_xml(ConnectInfo(addr): ConnectInfo<SocketAddr>) -> impl IntoResponse {
    info!(?addr, "request for description.xml");
    generate_description(&MEDIA_SERVER_DESCRIPTION).await
}

struct SsdpDiscoverResponse<'a> {
    cache_control_max_age: usize,
    date: SystemTime,
    location: &'a str,
    server: &'a str,
    usn: &'a str,
}

fn generate_ssdp_discover_response(response: &SsdpDiscoverResponse<'_>, st: &BStr) -> String {
    let cache_control_max_age = response.cache_control_max_age;
    // TODO: add DATE header
    let server = response.server;
    let usn = response.usn;
    let date = fmt_http_date(response.date);
    let location = response.location;

    let test = r#"HTTP/1.1 200 OK
Cache-Control: max-age=75
Ext:
Location: http://192.168.0.165:1338/rootDesc.xml
Server: Linux/3.4 DLNADOC/1.50 UPnP/1.0 dms/1
St: urn:schemas-upnp-org:device:MediaServer:1
Usn: uuid:c1aa84b5-0713-7606-a452-21c4f0483082::urn:schemas-upnp-org:device:MediaServer:1
Content-Length: 0

"#;

    format!(
        "HTTP/1.1 200 OK\r
Cache-Control: max-age={cache_control_max_age}\r
Ext:\r
Location: {location}\r
Server: {server}\r
St: {st}\r
Usn: {usn}::{st}\r
Content-Length: 0\r
\r
"
    )
}

const UPNP_KIND_ROOT_DEVICE: &str = "upnp:rootdevice";
const UPNP_KIND_MEDIASERVER: &str = "urn:schemas-upnp-org:device:MediaServer:1";

pub fn generate_ssdp_notify_message(usn: &str, kind: &str) -> String {
    let test = r#"NOTIFY * HTTP/1.1
HOST: 239.255.255.250:1900
NT: urn:schemas-upnp-org:device:MediaServer:1
NTS: ssdp:alive
SERVER: Linux/3.4 DLNADOC/1.50 UPnP/1.0 dms/1
USN: uuid:c1aa84b5-0713-7606-a452-21c4f0483082::urn:schemas-upnp-org:device:MediaServer:1
CACHE-CONTROL: max-age=75
LOCATION: http://192.168.0.165:1338/rootDesc.xml
"#;

    let server_string = SSDP_SERVER_STRING;

    format!(
        "NOTIFY * HTTP/1.1\r
Host: 239.255.255.250:1900\r
Cache-Control: max-age=75\r
Location: http://192.168.0.112:9005/description.xml\r
NT: {kind}\r
NTS: ssdp:alive\r
Server: {server_string}\r
USN: {usn}::{kind}\r
\r
"
    )
}

async fn generate_connection_manager_scpd(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> impl IntoResponse {
    info!(?addr, ?headers, "request to content directory SCPD");
    (
        [
            ("Content-Type", r#"text/xml; charset="utf-8""#),
            ("Server", MEDIA_SERVER_DESCRIPTION.server_string),
        ],
        include_str!("../resources/scpd_connection_manager.xml"),
    )
}
async fn generate_content_directory_scpd(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> impl IntoResponse {
    info!(?addr, ?headers, "request to content directory SCPD");
    (
        [
            ("Content-Type", r#"text/xml; charset="utf-8""#),
            ("Server", MEDIA_SERVER_DESCRIPTION.server_string),
        ],
        include_str!("../resources/ContentDirectorySCPD_dms.xml"),
    )
}

async fn generate_content_directory_control_response(
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let body = BStr::new(&body);
    debug!(?headers, ?body, "scpd request headers");

    let result = r#"
        <DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"
            xmlns:dc="http://purl.org/dc/elements/1.1/"
            xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">
          <item id="1" parentID="0" restricted="true">
            <dc:title>1.mkv</dc:title>
            <upnp:class>object.item.videoItem</upnp:class>
            <res protocolInfo="http-get:*:video/x-matroska:*">http://192.168.0.165:3030/torrents/4/stream/0/Despicable.Me.4.2024.1080p.WEB-DL.H264.SPh.mkv</res>
          </item>
          <item id="2" parentID="0" restricted="true">
            <dc:title>2.mkv</dc:title>
            <upnp:class>object.item.videoItem</upnp:class>
            <res protocolInfo="http-get:*:video/x-matroska:*">http://192.168.0.165:3030/torrents/5/stream/0/Twisters.2024.WEB-DL.2160p.HDR.DV.seleZen.mkv</res>
          </item>
        </DIDL-Lite>
        "#;

    let result = quick_xml::escape::escape(&result);
    let body = format!(
        r#"
        <?xml version="1.0" encoding="utf-8" standalone="yes"?>
        <s:Envelope
                xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"
                s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
            <s:Body>
                <u:BrowseResponse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
                    <Result>{result}</Result>
                    <NumberReturned>2</NumberReturned>
                    <TotalMatches>2</TotalMatches>
                    <UpdateID>11184</UpdateID>
                </u:BrowseResponse>
            </s:Body>
        </s:Envelope>
    "#
    );

    ([("Content-Type", "text/xml; charset=\"utf-8\"")], body)
}

async fn connection_manager_stub(headers: HeaderMap, body: Bytes) -> impl IntoResponse {
    info!(body=?BStr::new(&body), ?headers, "connection manager request");

    ""
}

async fn run_server(port: u16) -> anyhow::Result<()> {
    let app = axum::Router::new()
        .route("/description.xml", get(description_xml))
        .route(
            "/scpd/ContentDirectory.xml",
            get(generate_content_directory_scpd),
        )
        .route(
            "/scpd/ConnectionManager.xml",
            get(generate_connection_manager_scpd),
        )
        .route(
            "/control/ContentDirectory",
            post(generate_content_directory_control_response),
        )
        .route("/control/ConnectionManager", post(connection_manager_stub));

    let app = app
        .layer(TraceLayer::new_for_http())
        .into_make_service_with_connect_info::<SocketAddr>();

    // run our app with hyper, listening globally on port 3000
    let listener = tokio::net::TcpListener::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port))
        .await
        .context("error running listener")?;
    axum::serve(listener, app).await.context("error serving")?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    spawn(async {
        if let Err(e) = run_server(HTTP_PORT).await {
            error!(error=?e, "error running HTTP server")
        }
    });

    const UPNP_PORT: u16 = 1900;
    const UPNP_BROADCAST_IP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
    const UPNP_BROADCAST_ADDR: SocketAddrV4 = SocketAddrV4::new(UPNP_BROADCAST_IP, UPNP_PORT);

    let sock = tokio::net::UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, UPNP_PORT))
        .await
        .context("error binding")?;

    sock.join_multicast_v4(UPNP_BROADCAST_IP, Ipv4Addr::UNSPECIFIED)
        .context("error joining multicast group")?;

    for kind in [UPNP_KIND_ROOT_DEVICE, UPNP_KIND_MEDIASERVER] {
        let msg = generate_ssdp_notify_message(USN, kind);
        debug!(content=?msg, addr=?UPNP_BROADCAST_ADDR, "sending SSDP NOTIFY");
        sock.send_to(msg.as_bytes(), UPNP_BROADCAST_ADDR)
            .await
            .context("error sending notify")?;
    }

    let msearch_msg = format!(
        "M-SEARCH * HTTP/1.1\r
HOST: 239.255.255.250:1900\r
ST: urn:schemas-upnp-org:device:MediaServer:1\r
MAN: \"ssdp:discover\"\r
MX: 2\r\n\r\n"
    );
    sock.send_to(msearch_msg.as_bytes(), UPNP_BROADCAST_ADDR)
        .await
        .context("error sending msearch")?;

    let mut buf = vec![0u8; 16184];
    loop {
        debug!("trying to recv message");
        let (sz, addr) = sock.recv_from(&mut buf).await.context("error receiving")?;
        let msg = &buf[..sz];
        debug!(content = ?BStr::new(msg), ?addr, "received message");
        let parsed = try_parse_ssdp(msg);
        let msg = match parsed {
            Ok(SsdpMessage::MSearch(msg)) => {
                info!(?msg, "parsed");
                msg
            }
            Ok(m) => {
                debug!("ignoring {m:?}");
                continue;
            }
            Err(e) => {
                error!(error=?e, "error parsing SSDP message");
                continue;
            }
        };
        if !msg.matches_media_server() {
            continue;
        }

        let response = generate_ssdp_discover_response(
            &SsdpDiscoverResponse {
                cache_control_max_age: 75,
                date: SystemTime::now(),
                location: "http://192.168.0.112:9005/description.xml",
                server: SSDP_SERVER_STRING,
                usn: USN,
            },
            msg.st,
        );
        debug!(content = response, ?addr, "sending SSDP discover response");
        sock.send_to(response.as_bytes(), addr)
            .await
            .context("error sending")?;
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use crate::{
        generate_ssdp_discover_response, try_parse_ssdp, SsdpDiscoverResponse,
        MEDIA_SERVER_DESCRIPTION, UPNP_KIND_MEDIASERVER,
    };

    #[test]
    fn test_parse() {
        tracing_subscriber::fmt::init();

        let msg = b"M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: urn:dial-multiscreen-org:service:dial:1\r\nUSER-AGENT: Google Chrome/127.0.6533.100 Mac OS X\r\n\r\n";
        dbg!(try_parse_ssdp(msg).unwrap());
    }

    #[test]
    fn test_generate() {
        let resp = generate_ssdp_discover_response(
            &SsdpDiscoverResponse {
                cache_control_max_age: 1,
                date: SystemTime::now(),
                location: "http://192.168.0.112:9005/description.xml",
                server: MEDIA_SERVER_DESCRIPTION.friendly_name,
                usn: MEDIA_SERVER_DESCRIPTION.unique_id,
            },
            UPNP_KIND_MEDIASERVER.into(),
        );
        dbg!(&resp);
    }
}
