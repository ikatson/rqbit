use anyhow::{bail, Context};
use bstr::BStr;
use futures::{stream::FuturesUnordered, StreamExt, TryFutureExt};
use network_interface::NetworkInterfaceConfig;
use reqwest::Client;
use serde::Deserialize;
use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    time::Duration,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tracing::{debug, error_span, trace, warn, Instrument, Span};
use url::Url;

const SERVICE_TYPE_WAN_IP_CONNECTION: &str = "urn:schemas-upnp-org:service:WANIPConnection:1";
const SSDP_MULTICAST_IP: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900));
const SSDP_SEARCH_REQUEST: &str = "M-SEARCH * HTTP/1.1\r\n\
                                   Host: 239.255.255.250:1900\r\n\
                                   Man: \"ssdp:discover\"\r\n\
                                   MX: 3\r\n\
                                   ST: urn:schemas-upnp-org:service:WANIPConnection:1\r\n\
                                   \r\n";

pub fn get_local_ip_relative_to(local_dest: IpAddr) -> anyhow::Result<Ipv4Addr> {
    let local_dest = match local_dest {
        IpAddr::V4(v4) => v4,
        IpAddr::V6(v6) => {
            anyhow::bail!("get_local_ip_relative_to not implemented for IPv6; addr={v6}")
        }
    };

    // Ipv4Addr.to_bits() is only there in nightly rust, so copying here for now.
    fn ip_bits(addr: Ipv4Addr) -> u32 {
        u32::from_be_bytes(addr.octets())
    }

    fn masked(ip: Ipv4Addr, mask: Ipv4Addr) -> u32 {
        ip_bits(ip) & ip_bits(mask)
    }

    let interfaces =
        network_interface::NetworkInterface::show().context("error listing network interfaces")?;

    for i in interfaces {
        for addr in i.addr {
            if let network_interface::Addr::V4(v4) = addr {
                let ip = v4.ip;
                let mask = match v4.netmask {
                    Some(mask) => mask,
                    None => continue,
                };
                trace!("found local addr {ip}/{mask}");
                // If the masked address is the same, means we are on the same network, return
                // the ip address
                if masked(ip, mask) == masked(local_dest, mask) {
                    return Ok(ip);
                }
            }
        }
    }
    bail!("couldn't find a local ip address")
}

async fn forward_port(
    control_url: Url,
    local_ip: Ipv4Addr,
    port: u16,
    lease_duration: Duration,
) -> anyhow::Result<()> {
    let request_body = format!(
        r#"
        <s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"
            s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
            <s:Body>
                <u:AddPortMapping xmlns:u="{SERVICE_TYPE_WAN_IP_CONNECTION}">
                    <NewRemoteHost></NewRemoteHost>
                    <NewExternalPort>{port}</NewExternalPort>
                    <NewProtocol>TCP</NewProtocol>
                    <NewInternalPort>{port}</NewInternalPort>
                    <NewInternalClient>{local_ip}</NewInternalClient>
                    <NewEnabled>1</NewEnabled>
                    <NewPortMappingDescription>rust UPnP</NewPortMappingDescription>
                    <NewLeaseDuration>{}</NewLeaseDuration>
                </u:AddPortMapping>
            </s:Body>
        </s:Envelope>
    "#,
        lease_duration.as_secs()
    );

    let url = control_url;

    let client = reqwest::Client::new();
    let response = client
        .post(url.clone())
        .header("Content-Type", "text/xml")
        .header(
            "SOAPAction",
            format!("\"{}#AddPortMapping\"", SERVICE_TYPE_WAN_IP_CONNECTION),
        )
        .body(request_body)
        .send()
        .await
        .context("error sending")?;

    let status = response.status();

    let response_text = response
        .text()
        .await
        .context("error reading response text")?;

    trace!(status = %status, text=response_text, "AddPortMapping response");
    if !status.is_success() {
        bail!("failed port forwarding: {}", status);
    } else {
        debug!(%local_ip, port, "successfully port forwarded");
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct RootDesc {
    #[serde(rename = "device")]
    devices: Vec<Device>,
}

#[derive(Default, Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct DeviceList {
    #[serde(rename = "device")]
    devices: Vec<Device>,
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
        error_span!(parent: parent, "device", device = self.name())
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
}

impl Service {
    pub fn span(&self, parent: tracing::Span) -> tracing::Span {
        error_span!(parent: parent, "service", url = self.control_url)
    }
}

#[derive(Debug)]
struct UpnpEndpoint {
    discover_response: UpnpDiscoverResponse,
    data: RootDesc,
}

impl UpnpEndpoint {
    fn location(&self) -> &Url {
        &self.discover_response.location
    }

    fn span(&self) -> tracing::Span {
        error_span!("upnp_endpoint", location = %self.location())
    }

    fn iter_services(&self) -> impl Iterator<Item = (tracing::Span, &Service)> + '_ {
        let self_span = self.span();
        self.data
            .devices
            .iter()
            .flat_map(move |d| d.iter_services(self_span.clone()))
    }

    fn my_local_ip(&self) -> anyhow::Result<Ipv4Addr> {
        let dest_ip = self.discover_response.received_from.ip();
        let local_ip = get_local_ip_relative_to(dest_ip)
            .with_context(|| format!("can't determine local IP relative to {dest_ip}"))?;
        Ok(local_ip)
    }

    fn get_wan_ip_control_urls(&self) -> impl Iterator<Item = (tracing::Span, Url)> + '_ {
        self.iter_services()
            .filter(|(_, s)| s.service_type == SERVICE_TYPE_WAN_IP_CONNECTION)
            .map(|(span, s)| (span, self.discover_response.location.join(&s.control_url)))
            .filter_map(|(span, url)| match url {
                Ok(url) => Some((span, url)),
                Err(e) => {
                    debug!("bad control url: {e:#}");
                    None
                }
            })
    }
}

#[derive(Debug)]
struct UpnpDiscoverResponse {
    pub received_from: SocketAddr,
    pub location: Url,
}

async fn discover_services(location: Url) -> anyhow::Result<RootDesc> {
    let response = Client::new()
        .get(location.clone())
        .send()
        .await
        .context("failed to send GET request")?
        .text()
        .await
        .context("failed to read response body")?;
    trace!("received from {location}: {response}");
    let root_desc: RootDesc = quick_xml::de::from_str(&response)
        .context("failed to parse response body as xml")
        .map_err(|e| {
            debug!("failed to parse this XML: {response}");
            e
        })?;
    Ok(root_desc)
}

fn parse_upnp_discover_response(
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

pub struct UpnpPortForwarder {
    ports: Vec<u16>,
    opts: UpnpPortForwarderOptions,
}

impl UpnpPortForwarder {
    pub fn new(ports: Vec<u16>, opts: Option<UpnpPortForwarderOptions>) -> anyhow::Result<Self> {
        if ports.is_empty() {
            bail!("empty ports")
        }
        Ok(Self {
            ports,
            opts: opts.unwrap_or_default(),
        })
    }

    async fn parse_endpoint(
        &self,
        discover_response: UpnpDiscoverResponse,
    ) -> anyhow::Result<UpnpEndpoint> {
        let services = discover_services(discover_response.location.clone()).await?;
        Ok(UpnpEndpoint {
            discover_response,
            data: services,
        })
    }

    async fn discover_once(
        &self,
        tx: &UnboundedSender<UpnpDiscoverResponse>,
    ) -> anyhow::Result<()> {
        let socket = tokio::net::UdpSocket::bind("0.0.0.0:0")
            .await
            .context("failed to bind UDP socket")?;
        socket
            .send_to(SSDP_SEARCH_REQUEST.as_bytes(), SSDP_MULTICAST_IP)
            .await
            .context("failed to send SSDP search request")?;

        let mut buffer = [0; 2048];

        let timeout = tokio::time::sleep(self.opts.discover_timeout);
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
                        Err(e) => warn!(error=?e, response=?BStr::new(response), "failed to parse SSDP response"),
                    };
                },
            }
        }

        debug!("discovered {discovered} endpoints");
        Ok(())
    }

    async fn discovery(&self, tx: UnboundedSender<UpnpDiscoverResponse>) -> anyhow::Result<()> {
        let mut discover_interval = tokio::time::interval(self.opts.discover_interval);
        discover_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            discover_interval.tick().await;
            if let Err(e) = self.discover_once(&tx).await {
                warn!("failed to run discovery: {e:#}");
            }
        }
    }

    async fn manage_port(&self, control_url: Url, local_ip: Ipv4Addr, port: u16) -> ! {
        let lease_duration = self.opts.lease_duration;
        let mut interval = tokio::time::interval(lease_duration / 2);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            interval.tick().await;
            if let Err(e) = forward_port(control_url.clone(), local_ip, port, lease_duration).await
            {
                warn!("failed to forward port: {e:#}");
            }
        }
    }

    async fn manage_service(&self, control_url: Url, local_ip: Ipv4Addr) -> anyhow::Result<()> {
        futures::future::join_all(self.ports.iter().cloned().map(|port| {
            self.manage_port(control_url.clone(), local_ip, port)
                .instrument(error_span!("manage_port", port = port))
        }))
        .await;
        Ok(())
    }

    pub async fn run_forever(self) -> ! {
        let (discover_tx, mut discover_rx) = unbounded_channel();
        let discovery = self.discovery(discover_tx);

        let mut spawned_tasks = HashSet::<Url>::new();

        let mut endpoints = FuturesUnordered::new();
        let mut service_managers = FuturesUnordered::new();

        tokio::pin!(discovery);

        loop {
            tokio::select! {
                _ = &mut discovery => {},
                r = discover_rx.recv() => {
                    let r = r.unwrap();
                    let location = r.location.clone();
                    endpoints.push(self.parse_endpoint(r).map_err(|e| {
                        debug!("error parsing endpoint: {e:#}");
                        e
                    }).instrument(error_span!("parse endpoint", location=location.to_string())));
                },
                Some(Ok(endpoint)) = endpoints.next(), if !endpoints.is_empty() => {
                    let mut local_ip = None;
                    for (span, control_url) in endpoint.get_wan_ip_control_urls() {
                        if spawned_tasks.contains(&control_url) {
                            debug!("already spawned for {}", control_url);
                            continue;
                        }
                        let ip = match local_ip {
                            Some(ip) => ip,
                            None => {
                                match endpoint.my_local_ip() {
                                    Ok(ip) => {
                                        local_ip = Some(ip);
                                        ip
                                    },
                                    Err(e) => {
                                        warn!("failed to determine local IP for endpoint at {}: {:#}", endpoint.location(), e);
                                        break;
                                    }
                                }
                            }
                        };
                        spawned_tasks.insert(control_url.clone());
                        service_managers.push(self.manage_service(control_url, ip).instrument(span))
                    }
                },
                _ = service_managers.next(), if !service_managers.is_empty() => {

                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use quick_xml::de::from_str;

    use crate::{Device, DeviceList, RootDesc, Service, ServiceList};

    #[test]
    fn test_parse() {
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
