use anyhow::{Context, bail};
use bstr::BStr;
use librqbit_dualstack_sockets::{BindDevice, UdpSocket};
use network_interface::NetworkInterface;
use reqwest::Client;
use serde_derive::Deserialize;
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4},
    time::Duration,
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{Span, debug, debug_span, trace, warn};
use url::Url;

mod gateway;
mod igd;
mod mapper;
#[cfg(test)]
mod mapper_tests;
mod natpmp;
mod pcp;
#[cfg(any(test, feature = "test-util"))]
pub mod test_util;
mod transport;

pub use mapper::{
    NATPMP_PCP_PORT, PortMapper, PortMappingMethod, PortMappingMethods, PortMappingOptions,
    PortMappingProtocol, PortMappingState, PortMappingStatus,
};

const SSDP_MULTICAST_IP: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900));
pub const SSDP_SEARCH_WAN_IPCONNECTION_ST: &str = "urn:schemas-upnp-org:service:WANIPConnection:1";
pub const SSDP_SEARCH_ROOT_ST: &str = "upnp:rootdevice";

pub fn make_ssdp_search_request(kind: &str) -> String {
    format!(
        "M-SEARCH * HTTP/1.1\r\n\
            Host: 239.255.255.250:1900\r\n\
            Man: \"ssdp:discover\"\r\n\
            MX: 3\r\n\
            ST: {kind}\r\n\
            \r\n"
    )
}

pub fn get_local_ip_relative_to(
    local_dest: SocketAddr,
    interfaces: &[NetworkInterface],
) -> anyhow::Result<IpAddr> {
    fn masked_v4(ip: Ipv4Addr, mask: Ipv4Addr) -> u32 {
        ip.to_bits() & mask.to_bits()
    }

    fn masked_v6(ip: Ipv6Addr, mask: Ipv6Addr) -> u128 {
        ip.to_bits() & mask.to_bits()
    }

    for i in interfaces {
        for addr in i.addr.iter() {
            match (local_dest, addr.ip(), addr.netmask()) {
                // We are connecting to ourselves, return itself.
                (l, a, _) if l.ip() == a => return Ok(addr.ip()),
                // IPv4 masks match.
                (SocketAddr::V4(l), IpAddr::V4(a), Some(IpAddr::V4(m)))
                    if masked_v4(*l.ip(), m) == masked_v4(a, m) =>
                {
                    return Ok(addr.ip());
                }
                // Return IPv6 link-local addresses when source is link-local address and there's a scope_id set.
                (SocketAddr::V6(l), IpAddr::V6(a), _)
                    if l.ip().is_unicast_link_local() && l.scope_id() > 0 =>
                {
                    if a.is_unicast_link_local() && l.scope_id() == i.index {
                        return Ok(addr.ip());
                    }
                }
                // If V6 masks match, return.
                (SocketAddr::V6(l), IpAddr::V6(a), Some(IpAddr::V6(m)))
                    if masked_v6(*l.ip(), m) == masked_v6(a, m) =>
                {
                    return Ok(addr.ip());
                }
                // For IPv6 fallback to returning a random (first encountered) IPv6 address.
                (SocketAddr::V6(_), IpAddr::V6(_), None) => return Ok(addr.ip()),
                _ => continue,
            }
        }
    }
    bail!("couldn't find a local ip address")
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct RootDesc {
    #[serde(rename = "device")]
    pub devices: Vec<Device>,
}

#[derive(Default, Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct DeviceList {
    #[serde(rename = "device")]
    pub devices: Vec<Device>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Device {
    #[serde(rename = "deviceType")]
    pub device_type: String,
    #[serde(rename = "friendlyName", default)]
    pub friendly_name: String,
    #[serde(rename = "serviceList", default)]
    pub service_list: ServiceList,
    #[serde(rename = "deviceList", default)]
    pub device_list: DeviceList,
}

impl Device {
    pub fn iter_services(
        &self,
        parent: Span,
    ) -> Box<dyn Iterator<Item = (tracing::Span, &Service)> + '_> {
        let self_span = self.span(parent);
        let services = self.service_list.services.iter().map({
            let self_span = self_span.clone();
            move |s| (s.span(self_span.clone()), s)
        });
        Box::new(services.chain(self.device_list.devices.iter().flat_map({
            let self_span = self_span.clone();
            move |d| d.iter_services(self_span.clone())
        })))
    }

    pub fn span(&self, parent: tracing::Span) -> tracing::Span {
        debug_span!(parent: parent, "device", device = self.name())
    }
}

impl Device {
    pub fn name(&self) -> &str {
        if self.friendly_name.is_empty() {
            return &self.device_type;
        }
        &self.friendly_name
    }
}

#[derive(Clone, Debug, Deserialize, Default, PartialEq, Eq)]
pub struct ServiceList {
    #[serde(rename = "service", default)]
    pub services: Vec<Service>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Service {
    #[serde(rename = "serviceType")]
    pub service_type: String,
    #[serde(rename = "controlURL")]
    pub control_url: String,
    #[serde(rename = "SCPDURL")]
    pub scpd_url: String,
    #[serde(rename = "eventSubURL", default)]
    pub event_sub_url: Option<String>,
}

impl Service {
    pub fn span(&self, parent: tracing::Span) -> tracing::Span {
        debug_span!(parent: parent, "service", url = self.control_url)
    }
}

#[derive(Debug)]
pub struct UpnpDiscoverResponse {
    pub received_from: SocketAddr,
    pub location: Url,
}

pub async fn discover_services(location: Url) -> anyhow::Result<RootDesc> {
    fetch_root_desc(&Client::new(), &location).await
}

/// Downloads and parses the device description at `location`.
pub(crate) async fn fetch_root_desc(client: &Client, location: &Url) -> anyhow::Result<RootDesc> {
    let response = client
        .get(location.clone())
        .send()
        .await
        .context("failed to send GET request")?
        .error_for_status()
        .context("device description request failed")?
        .text()
        .await
        .context("failed to read response body")?;
    trace!("received from {location}: {response}");
    let root_desc: RootDesc = quick_xml::de::from_str(&response)
        .context("failed to parse response body as xml")
        .inspect_err(|e| {
            debug!("failed to parse this XML: {response}. Error: {e:#}");
        })?;
    Ok(root_desc)
}

pub fn parse_upnp_discover_response(
    buf: &[u8],
    received_from: SocketAddr,
) -> anyhow::Result<UpnpDiscoverResponse> {
    let mut headers = [httparse::EMPTY_HEADER; 16];
    let mut resp = httparse::Response::new(&mut headers);
    resp.parse(buf).context("error parsing response")?;

    trace!(?resp, "parsed SSDP response");
    match resp.code {
        Some(200) => {}
        other => anyhow::bail!("bad response code {other:?}, expected 200"),
    }
    let mut location = None;
    for header in resp.headers {
        match header.name {
            "location" | "LOCATION" | "Location" => {
                location = Some(
                    std::str::from_utf8(header.value).context("bad utf-8 in location header")?,
                )
            }
            _ => continue,
        }
    }
    let location = location.context("missing location header")?;
    let location =
        Url::parse(location).with_context(|| format!("failed parsing location {location}"))?;
    Ok(UpnpDiscoverResponse {
        location,
        received_from,
    })
}

/// Sends one SSDP search along the default multicast route (or out of `bind_device`) and reports
/// every answer received within `timeout`.
pub async fn discover_once(
    tx: &UnboundedSender<UpnpDiscoverResponse>,
    kind: &str,
    timeout: Duration,
    bind_device: Option<&BindDevice>,
) -> anyhow::Result<()> {
    // TODO: do we need IPv6 support here? I can't test it, don't have the hardware for it (router / IPv6 provider).
    let socket = UdpSocket::bind_udp(
        (Ipv4Addr::UNSPECIFIED, 0).into(),
        librqbit_dualstack_sockets::BindOpts {
            device: bind_device,
            ..Default::default()
        },
    )?;
    search(socket.socket(), tx, kind, timeout).await
}

/// Like [`discover_once`], but searches through the interface that owns `source`: the socket is
/// bound to that address and multicasts out of its interface (`IP_MULTICAST_IF`), whatever the
/// default multicast route is. A VPN or a TUN proxy that owns the default route never delivers
/// the search to the router of the local network.
pub async fn discover_once_from(
    tx: &UnboundedSender<UpnpDiscoverResponse>,
    kind: &str,
    timeout: Duration,
    source: Ipv4Addr,
) -> anyhow::Result<()> {
    let socket = tokio::net::UdpSocket::bind((source, 0))
        .await
        .with_context(|| format!("failed to bind an SSDP socket to {source}"))?;
    socket2::SockRef::from(&socket)
        .set_multicast_if_v4(&source)
        .with_context(|| format!("failed to send multicast out of {source}"))?;
    search(&socket, tx, kind, timeout).await
}

async fn search(
    socket: &tokio::net::UdpSocket,
    tx: &UnboundedSender<UpnpDiscoverResponse>,
    kind: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    let message = make_ssdp_search_request(kind);
    socket
        .send_to(message.as_bytes(), SSDP_MULTICAST_IP)
        .await
        .with_context(|| format!("failed to send SSDP search request to {SSDP_MULTICAST_IP}"))?;

    let mut buffer = [0; 2048];

    let timeout = tokio::time::sleep(timeout);
    let mut timed_out = false;
    tokio::pin!(timeout);

    let mut discovered = 0;

    while !timed_out {
        tokio::select! {
            _ = &mut timeout, if !timed_out => {
                timed_out = true;
            }
            Ok((len, addr)) = socket.recv_from(&mut buffer), if !timed_out => {
                let response = &buffer[..len];
                match parse_upnp_discover_response(response, addr) {
                    Ok(r) => {
                        tx.send(r)?;
                        discovered += 1;
                    },
                    Err(e) => warn!(response=?BStr::new(response), "failed to parse SSDP response: {e:#}"),
                };
            },
        }
    }

    debug!("discovered {discovered} endpoints");
    Ok(())
}

/// Settings of [`UpnpPortForwarder`].
pub struct UpnpPortForwarderOptions {
    pub lease_duration: Duration,
    pub discover_interval: Duration,
    pub discover_timeout: Duration,
}

impl Default for UpnpPortForwarderOptions {
    fn default() -> Self {
        Self {
            discover_interval: Duration::from_secs(60),
            discover_timeout: Duration::from_secs(10),
            lease_duration: Duration::from_secs(60),
        }
    }
}

/// Maps TCP ports with UPnP for as long as it runs. A [`PortMapper`] limited to UPnP and TCP;
/// use [`PortMapper`] directly for UDP, NAT-PMP / PCP, status and deletion on shutdown.
pub struct UpnpPortForwarder {
    mapper: PortMapper,
}

impl UpnpPortForwarder {
    pub fn new(
        ports: Vec<u16>,
        opts: Option<UpnpPortForwarderOptions>,
        bind_device: Option<BindDevice>,
    ) -> anyhow::Result<Self> {
        if ports.is_empty() {
            bail!("empty ports")
        }
        let opts = opts.unwrap_or_default();
        let mapper = PortMapper::new(
            ports.into_iter().map(|p| (PortMappingProtocol::Tcp, p)),
            PortMappingMethods {
                upnp: true,
                natpmp: false,
            },
            PortMappingOptions {
                lease_duration: opts.lease_duration,
                discover_interval: opts.discover_interval,
                discover_timeout: opts.discover_timeout,
                ..Default::default()
            },
            bind_device,
        );
        Ok(Self { mapper })
    }

    /// The mapper behind this forwarder, to read its status.
    pub fn mapper(&self) -> &PortMapper {
        &self.mapper
    }

    pub async fn run_forever(self) -> ! {
        self.mapper.run(std::future::pending()).await;
        // `run` returns only once its shutdown future resolves, and this one never does.
        loop {
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use quick_xml::de::from_str;

    use crate::{Device, DeviceList, RootDesc, Service, ServiceList};

    #[test]
    fn test_parse_root_desc() {
        let actual = from_str::<RootDesc>(include_str!("resources/test/devices-0.xml")).unwrap();
        let expected = RootDesc {
            devices: vec![Device {
                device_type: "urn:schemas-upnp-org:device:InternetGatewayDevice:1".into(),
                friendly_name: "ARRIS TG3492LG".into(),
                service_list: ServiceList {
                    services: vec![Service {
                        service_type: "urn:schemas-upnp-org:service:Layer3Forwarding:1".into(),
                        control_url: "/upnp/control/Layer3Forwarding".into(),
                        scpd_url: "/Layer3ForwardingSCPD.xml".into(),
                        event_sub_url: Some("/upnp/event/Layer3Forwarding".into()),
                    }],
                },
                device_list: DeviceList {
                    devices: vec![Device {
                        device_type: "urn:schemas-upnp-org:device:WANDevice:1".into(),
                        friendly_name: "WANDevice:1".into(),
                        service_list: ServiceList {
                            services: vec![Service {
                                service_type:
                                    "urn:schemas-upnp-org:service:WANCommonInterfaceConfig:1".into(),
                                control_url: "/upnp/control/WANCommonInterfaceConfig0".into(),
                                scpd_url: "/WANCommonInterfaceConfigSCPD.xml".into(),
                                event_sub_url: Some("/upnp/event/WANCommonInterfaceConfig0".into()),
                            }],
                        },
                        device_list: DeviceList {
                            devices: vec![Device {
                                device_type: "urn:schemas-upnp-org:device:WANConnectionDevice:1"
                                    .into(),
                                friendly_name: "WANConnectionDevice:1".into(),
                                service_list: ServiceList {
                                    services: vec![Service {
                                        service_type:
                                            "urn:schemas-upnp-org:service:WANIPConnection:1".into(),
                                        control_url: "/upnp/control/WANIPConnection0".into(),
                                        scpd_url: "/WANIPConnectionServiceSCPD.xml".into(),
                                        event_sub_url: Some("/upnp/event/WANIPConnection0".into()),
                                    }],
                                },
                                device_list: DeviceList { devices: vec![] },
                            }],
                        },
                    }],
                },
            }],
        };
        assert_eq!(actual, expected);
    }
}
