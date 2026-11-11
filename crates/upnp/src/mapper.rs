//! One mapper for UPnP IGD, PCP and NAT-PMP.
//!
//! The mapper keeps a set of `(protocol, internal port)` mapped on every router it finds. UPnP and
//! NAT-PMP / PCP run side by side; a port counts as mapped as soon as one of them mapped it. Each
//! router is a "source" with its own outcome per port, and [`PortMapper::status`] folds them into
//! one row per port.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use futures::{StreamExt, stream::FuturesUnordered};
use librqbit_dualstack_sockets::BindDevice;
use network_interface::{NetworkInterface, NetworkInterfaceConfig};
use reqwest::Client;
use serde_derive::{Deserialize, Serialize};
use tokio::{
    sync::mpsc::{UnboundedSender, unbounded_channel},
    time::Instant,
};
use tracing::{Instrument, debug, debug_span, info, trace, warn};
use url::Url;

use crate::{
    RootDesc, UpnpDiscoverResponse, discover_once, fetch_root_desc, gateway,
    get_local_ip_relative_to,
    igd::{self, IgdService, SoapError},
    natpmp, pcp,
    transport::{EpochRule, EpochTracker, ExchangeError, GatewaySocket, Retransmit},
};

/// Transport protocol of a mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortMappingProtocol {
    Tcp,
    Udp,
}

/// How a mapping was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortMappingMethod {
    /// UPnP IGD (`WANIPConnection` / `WANPPPConnection`).
    Upnp,
    /// NAT-PMP (RFC 6886).
    NatPmp,
    /// PCP (RFC 6887).
    Pcp,
}

/// Where a port stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortMappingState {
    /// Still looking for a router, or waiting for its answer.
    Pending,
    /// At least one method mapped the port.
    Mapped,
    /// No method mapped it; the text says why, per method and router.
    Failed(String),
}

/// One row of [`PortMapper::status`]: the outcome for one `(protocol, internal port)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortMappingStatus {
    pub protocol: PortMappingProtocol,
    pub internal_port: u16,
    /// The port on the router; differs from the internal one when the router picked another.
    pub external_port: Option<u16>,
    pub method: Option<PortMappingMethod>,
    pub external_ip: Option<IpAddr>,
    pub state: PortMappingState,
}

/// Which methods the mapper uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PortMappingMethods {
    /// UPnP IGD.
    pub upnp: bool,
    /// NAT-PMP, and PCP on the same gateways (PCP is tried first, NAT-PMP is the fallback).
    pub natpmp: bool,
}

/// RFC 6886 section 3.1 and RFC 6887 section 7: both protocols use UDP port 5351 on the gateway.
pub const NATPMP_PCP_PORT: u16 = 5351;

/// Settings of [`PortMapper`]. The defaults are right for real routers; most fields only exist so
/// that tests can point the mapper at fakes and run its timers fast.
#[derive(Debug, Clone)]
pub struct PortMappingOptions {
    /// NAT-PMP / PCP gateways to use. Empty: discover them (IPv4 only; see
    /// `gateway::gateway_candidates` for how). IPv6 addresses are spoken to with PCP only and
    /// open a firewall pinhole for our address instead of a NAT mapping.
    pub gateways: Vec<IpAddr>,
    /// UDP port of NAT-PMP / PCP gateways: [`NATPMP_PCP_PORT`] on real routers.
    pub gateway_port: u16,
    /// UPnP device description URLs to use instead of searching with SSDP.
    pub upnp_locations: Vec<Url>,
    /// `NewPortMappingDescription` of UPnP mappings, shown in the router's port forwarding page.
    pub description: String,
    /// Lifetime asked for every mapping. Mappings are renewed at half the granted lifetime.
    pub lease_duration: Duration,
    /// How often to look for routers again (SSDP, gateway probing), and how long to wait before
    /// retrying a mapping the router refused.
    pub discover_interval: Duration,
    /// How long one discovery round waits for answers. Also the timeout of UPnP HTTP requests.
    pub discover_timeout: Duration,
    /// NAT-PMP / PCP retransmission: the first wait, doubled after every attempt (RFC 6886
    /// section 3.1 says 250 ms).
    pub retransmit_initial: Duration,
    /// NAT-PMP / PCP retransmission: how many times a request is sent before the gateway counts
    /// as gone (RFC 6886 section 3.1 says 9).
    pub retransmit_attempts: u32,
    /// How long one second of router time lasts for the timers derived from router answers
    /// (granted lifetimes, epochs). One second except in tests, which shrink it to run fast.
    pub protocol_second: Duration,
    /// Upper bound for deleting the mappings when the mapper stops.
    pub shutdown_timeout: Duration,
}

impl Default for PortMappingOptions {
    fn default() -> Self {
        Self {
            gateways: Vec::new(),
            gateway_port: NATPMP_PCP_PORT,
            upnp_locations: Vec::new(),
            description: "rqbit".to_owned(),
            lease_duration: Duration::from_secs(3600),
            discover_interval: Duration::from_secs(60),
            discover_timeout: Duration::from_secs(10),
            retransmit_initial: Duration::from_millis(250),
            retransmit_attempts: 9,
            protocol_second: Duration::from_secs(1),
            shutdown_timeout: Duration::from_secs(2),
        }
    }
}

/// Lifetimes above this are treated as this long when scheduling renewals (RFC 6887 section 8.3
/// asks clients to cap absurd values).
const MAX_LIFETIME_SECS: u32 = 24 * 3600;

/// RFC 6887 section 9: after a gateway turned PCP down, try PCP again after 30 minutes in case
/// it was upgraded.
const PCP_RETRY_SECS: u32 = 30 * 60;

type PortKey = (PortMappingProtocol, u16);

/// A router that maps ports, as a key of the status table.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Source {
    /// A UPnP service, by control URL.
    Upnp(Url),
    /// A NAT-PMP / PCP gateway.
    Gateway(SocketAddr),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Pending,
    Mapped {
        external_port: u16,
        external_ip: Option<IpAddr>,
    },
    Failed(String),
}

#[derive(Debug, Clone)]
struct SourceEntry {
    method: PortMappingMethod,
    outcome: Outcome,
}

/// Where the search for routers of one method stands.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Discovery {
    Disabled,
    Searching,
    Found,
    NotFound(String),
    Stopped,
}

/// What we need to delete a gateway's mappings at shutdown, and to keep the same PCP nonces when
/// the gateway is found again.
#[derive(Debug, Clone)]
struct GatewayMemory {
    pcp: bool,
    /// The ports the gateway granted at some point: the ones to delete. Refused ports are not
    /// ours to delete.
    mapped: BTreeSet<PortKey>,
    nonces: BTreeMap<PortKey, pcp::Nonce>,
}

#[derive(Debug)]
struct State {
    rows: BTreeMap<PortKey, BTreeMap<Source, SourceEntry>>,
    upnp: Discovery,
    gateways: Discovery,
    /// IPv4 hosts of the internet gateway devices SSDP found: likely NAT-PMP / PCP gateways too.
    igd_hosts: Vec<Ipv4Addr>,
    upnp_mapped: Vec<(IgdService, PortKey)>,
    gateway_memory: BTreeMap<SocketAddr, GatewayMemory>,
}

fn method_rank(m: PortMappingMethod) -> u8 {
    match m {
        PortMappingMethod::Upnp => 0,
        PortMappingMethod::Pcp => 1,
        PortMappingMethod::NatPmp => 2,
    }
}

fn method_name(m: PortMappingMethod) -> &'static str {
    match m {
        PortMappingMethod::Upnp => "UPnP",
        PortMappingMethod::NatPmp => "NAT-PMP",
        PortMappingMethod::Pcp => "PCP",
    }
}

impl State {
    /// Folds the sources of one port into its row.
    ///
    /// When several methods mapped the port, the row shows the first of UPnP, PCP, NAT-PMP
    /// (IPv4 before IPv6 within one method). UPnP comes first because the mapper only accepts
    /// UPnP mappings whose external port is the internal one, so that row is exactly what peers
    /// can reach; PCP comes before NAT-PMP because it supersedes it.
    fn row(&self, key: PortKey, sources: &BTreeMap<Source, SourceEntry>) -> PortMappingStatus {
        let mapped = sources
            .values()
            .filter_map(|e| match &e.outcome {
                Outcome::Mapped {
                    external_port,
                    external_ip,
                } => Some((e.method, *external_port, *external_ip)),
                _ => None,
            })
            .min_by_key(|(method, _, ip)| {
                (method_rank(*method), ip.is_some_and(|ip| ip.is_ipv6()))
            });
        let (protocol, internal_port) = key;
        if let Some((method, external_port, external_ip)) = mapped {
            return PortMappingStatus {
                protocol,
                internal_port,
                external_port: Some(external_port),
                method: Some(method),
                external_ip,
                state: PortMappingState::Mapped,
            };
        }
        let pending = sources.values().any(|e| e.outcome == Outcome::Pending)
            || self.upnp == Discovery::Searching
            || self.gateways == Discovery::Searching;
        let state = if pending {
            PortMappingState::Pending
        } else {
            let reasons: Vec<&str> = sources
                .values()
                .filter_map(|e| match &e.outcome {
                    Outcome::Failed(why) => Some(why.as_str()),
                    _ => None,
                })
                .chain(
                    [&self.upnp, &self.gateways]
                        .into_iter()
                        .filter_map(|d| match d {
                            Discovery::NotFound(why) => Some(why.as_str()),
                            _ => None,
                        }),
                )
                .collect();
            if reasons.is_empty() {
                if self.upnp == Discovery::Stopped || self.gateways == Discovery::Stopped {
                    PortMappingState::Failed("port mapping stopped".to_owned())
                } else {
                    PortMappingState::Pending
                }
            } else {
                PortMappingState::Failed(reasons.join("; "))
            }
        };
        PortMappingStatus {
            protocol,
            internal_port,
            external_port: None,
            method: None,
            external_ip: None,
            state,
        }
    }
}

struct Inner {
    ports: Vec<PortKey>,
    methods: PortMappingMethods,
    opts: PortMappingOptions,
    bind_device: Option<BindDevice>,
    state: Mutex<State>,
}

/// Keeps ports mapped on the routers of the local network with UPnP IGD, PCP and NAT-PMP, and
/// reports how that goes. Cloning gives another handle to the same mapper.
#[derive(Clone)]
pub struct PortMapper {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for PortMapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortMapper")
            .field("ports", &self.inner.ports)
            .field("methods", &self.inner.methods)
            .finish_non_exhaustive()
    }
}

impl PortMapper {
    /// Creates a mapper for `ports`. Nothing happens until [`PortMapper::run`].
    pub fn new(
        ports: impl IntoIterator<Item = (PortMappingProtocol, u16)>,
        methods: PortMappingMethods,
        opts: PortMappingOptions,
        bind_device: Option<BindDevice>,
    ) -> Self {
        let mut ports: Vec<PortKey> = ports.into_iter().collect();
        ports.sort_unstable();
        ports.dedup();
        let discovery = |enabled: bool| {
            if enabled {
                Discovery::Searching
            } else {
                Discovery::Disabled
            }
        };
        let state = State {
            rows: ports.iter().map(|k| (*k, BTreeMap::new())).collect(),
            upnp: discovery(methods.upnp),
            gateways: discovery(methods.natpmp),
            igd_hosts: Vec::new(),
            upnp_mapped: Vec::new(),
            gateway_memory: BTreeMap::new(),
        };
        Self {
            inner: Arc::new(Inner {
                ports,
                methods,
                opts,
                bind_device,
                state: Mutex::new(state),
            }),
        }
    }

    /// One row per `(protocol, internal port)`, sorted by protocol then port.
    pub fn status(&self) -> Vec<PortMappingStatus> {
        let state = self.inner.lock();
        state
            .rows
            .iter()
            .map(|(key, sources)| state.row(*key, sources))
            .collect()
    }

    /// Maps the ports until `shutdown` resolves, then deletes what was mapped (giving up after
    /// `PortMappingOptions::shutdown_timeout`) and returns.
    pub async fn run(&self, shutdown: impl Future<Output = ()>) {
        let inner = &*self.inner;
        if !inner.ports.is_empty() {
            let upnp = async {
                if inner.methods.upnp {
                    inner.run_upnp().await
                } else {
                    std::future::pending().await
                }
            };
            let gateways = async {
                if inner.methods.natpmp {
                    inner.run_gateways().await
                } else {
                    std::future::pending().await
                }
            };
            tokio::select! {
                _ = futures::future::join(upnp, gateways) => {},
                _ = shutdown => {},
            }
        } else {
            shutdown.await;
        }
        inner.shutdown().await;
    }
}

/// The result of one mapping request.
enum Granted {
    Mapped {
        external_port: u16,
        external_ip: Option<IpAddr>,
        lifetime: u32,
    },
    Refused {
        why: String,
        retry_after: Duration,
    },
}

/// Why a PCP request did not produce a result.
enum PcpFailure {
    /// The gateway does not speak PCP version 2.
    Unsupported,
    Silent(ExchangeError),
}

enum PcpAnswer {
    Unsupported,
    Response(pcp::Response),
}

/// Picks the answer to `req` out of a datagram (RFC 6887 sections 8.3 and 11.4): the MAP fields
/// must match unless the answer is an error too short to carry them. A NAT-PMP gateway answers a
/// PCP request with a NAT-PMP `UNSUPP_VERSION` (RFC 6887 appendix A).
fn pcp_answer(buf: &[u8], req: &pcp::MapRequest) -> Option<PcpAnswer> {
    if buf.first() == Some(&natpmp::VERSION) {
        return natpmp::Response::decode(buf)
            .ok()
            .filter(|r| r.result == natpmp::ResultCode::UnsupportedVersion)
            .map(|_| PcpAnswer::Unsupported);
    }
    let r = pcp::Response::decode(buf).ok()?;
    if r.result == pcp::ResultCode::UnsuppVersion {
        return Some(PcpAnswer::Unsupported);
    }
    match &r.map {
        Some(m)
            if m.nonce != req.nonce
                || m.protocol != req.protocol
                || m.internal_port != req.internal_port =>
        {
            None
        }
        _ => Some(PcpAnswer::Response(r)),
    }
}

/// Picks the NAT-PMP answer to a mapping request out of a datagram.
fn natpmp_map_answer(
    buf: &[u8],
    protocol: PortMappingProtocol,
    internal_port: u16,
) -> Option<natpmp::Response> {
    let r = natpmp::Response::decode(buf).ok()?;
    if r.opcode != natpmp::map_opcode(protocol) {
        return None;
    }
    match r.body {
        natpmp::ResponseBody::Map {
            internal_port: p, ..
        } if p != internal_port => None,
        natpmp::ResponseBody::PublicAddress(_) => None,
        _ => Some(r),
    }
}

fn natpmp_public_answer(buf: &[u8]) -> Option<natpmp::Response> {
    natpmp::Response::decode(buf)
        .ok()
        .filter(|r| r.opcode == natpmp::OP_PUBLIC_ADDRESS)
}

/// Random enough for a PCP mapping nonce (RFC 6887 section 11.1: it only has to be hard to guess
/// for someone off the path) without a dependency: `RandomState` is seeded from the OS.
fn random_nonce() -> pcp::Nonce {
    use std::hash::{BuildHasher, Hasher};
    let state = std::collections::hash_map::RandomState::new();
    let mut nonce = [0u8; pcp::NONCE_LEN];
    for (i, chunk) in nonce.chunks_mut(8).enumerate() {
        let mut h = state.build_hasher();
        h.write_usize(i);
        for (dst, src) in chunk.iter_mut().zip(h.finish().to_ne_bytes()) {
            *dst = src;
        }
    }
    nonce
}

/// Why a gateway stopped being served.
struct GatewayEnd {
    /// Whether it granted anything before: if it did, look for it again at once.
    had_success: bool,
    why: String,
}

/// A message from UPnP discovery to the UPnP loop.
enum UpnpEvent {
    Device {
        services: Vec<IgdService>,
        local_ip: IpAddr,
    },
    RoundDone,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        // The state is plain data that every writer leaves consistent, so a panic elsewhere
        // does not make it unusable.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn retransmit(&self) -> Retransmit {
        Retransmit {
            initial: self.opts.retransmit_initial,
            attempts: self.opts.retransmit_attempts.max(1),
        }
    }

    fn requested_lifetime(&self) -> u32 {
        u32::try_from(self.opts.lease_duration.as_secs())
            .unwrap_or(u32::MAX)
            .max(1)
    }

    /// `secs` seconds of router time.
    fn router_secs(&self, secs: u32) -> Duration {
        self.opts.protocol_second.saturating_mul(secs)
    }

    /// When to renew a mapping granted for `lifetime` seconds: at half of it.
    fn renew_after(&self, lifetime: u32) -> Duration {
        self.router_secs(lifetime.min(MAX_LIFETIME_SECS)) / 2
    }

    fn set_outcome(
        &self,
        key: PortKey,
        source: &Source,
        method: PortMappingMethod,
        outcome: Outcome,
    ) {
        let mut state = self.lock();
        let Some(row) = state.rows.get_mut(&key) else {
            return;
        };
        let previous = row.insert(
            source.clone(),
            SourceEntry {
                method,
                outcome: outcome.clone(),
            },
        );
        if previous.is_some_and(|p| p.outcome == outcome && p.method == method) {
            trace!(?key, ?source, ?outcome, "port mapping renewed");
            return;
        }
        let (protocol, port) = key;
        match &outcome {
            Outcome::Mapped {
                external_port,
                external_ip,
            } => info!(
                ?protocol,
                port,
                external_port,
                ?external_ip,
                method = method_name(method),
                ?source,
                "port mapped"
            ),
            Outcome::Failed(why) => {
                warn!(?protocol, port, ?source, "port mapping failed: {why}")
            }
            Outcome::Pending => {}
        }
    }

    fn remove_sources(&self, mut matches: impl FnMut(&Source) -> bool) {
        let mut state = self.lock();
        for row in state.rows.values_mut() {
            row.retain(|s, _| !matches(s));
        }
    }

    fn http_client(&self) -> anyhow::Result<Client> {
        // Routers are on the local network: never send these through a proxy from the
        // environment.
        Ok(Client::builder()
            .no_proxy()
            .timeout(self.opts.discover_timeout)
            .build()?)
    }

    // ---------------------------------------------------------------------------------------
    // UPnP

    async fn run_upnp(&self) -> ! {
        let client = match self.http_client() {
            Ok(c) => c,
            Err(e) => {
                self.lock().upnp = Discovery::NotFound(format!("UPnP: no HTTP client: {e:#}"));
                loop {
                    std::future::pending::<()>().await;
                }
            }
        };
        let (tx, mut rx) = unbounded_channel();
        let discovery = self.upnp_discovery(&client, tx);
        tokio::pin!(discovery);
        // Discovery only ends when nobody listens, which cannot happen while this loop runs;
        // the flag keeps the finished future from being polled again all the same.
        let mut discovery_done = false;
        let mut managed = HashSet::<Url>::new();
        let mut services = FuturesUnordered::new();
        loop {
            tokio::select! {
                _ = &mut discovery, if !discovery_done => discovery_done = true,
                Some(event) = rx.recv() => match event {
                    UpnpEvent::Device { services: found, local_ip } => {
                        for service in found {
                            if managed.insert(service.control_url.clone()) {
                                let span = debug_span!("upnp_service", url = %service.control_url);
                                services.push(
                                    self.run_upnp_service(&client, service, local_ip).instrument(span),
                                );
                            }
                        }
                    }
                    UpnpEvent::RoundDone => {
                        let mut state = self.lock();
                        state.upnp = if managed.is_empty() {
                            Discovery::NotFound("UPnP: no internet gateway device found".to_owned())
                        } else {
                            Discovery::Found
                        };
                    }
                },
                Some(lost) = services.next(), if !services.is_empty() => {
                    managed.remove(&lost);
                    self.remove_sources(|s| *s == Source::Upnp(lost.clone()));
                    if managed.is_empty() {
                        self.lock().upnp = Discovery::Searching;
                    }
                }
            }
        }
    }

    /// Looks for internet gateway devices every `discover_interval` and reports their WAN
    /// connection services.
    async fn upnp_discovery(&self, client: &Client, tx: UnboundedSender<UpnpEvent>) {
        let mut interval = tokio::time::interval(self.opts.discover_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let responses = self.upnp_search().await;
            let mut seen = HashSet::new();
            for response in responses {
                if !seen.insert(response.location.clone()) {
                    continue;
                }
                let location = response.location.clone();
                match self.upnp_device(client, response).await {
                    Ok(Some(event)) => {
                        if tx.send(event).is_err() {
                            return;
                        }
                    }
                    Ok(None) => debug!(%location, "no WAN connection service"),
                    Err(e) => debug!(%location, "error reading device description: {e:#}"),
                }
            }
            if tx.send(UpnpEvent::RoundDone).is_err() {
                return;
            }
        }
    }

    /// One SSDP search for the three service types, or the configured locations.
    async fn upnp_search(&self) -> Vec<UpnpDiscoverResponse> {
        if !self.opts.upnp_locations.is_empty() {
            return self
                .opts
                .upnp_locations
                .iter()
                .filter_map(|location| {
                    let ip: IpAddr = match location.host()? {
                        url::Host::Ipv4(ip) => ip.into(),
                        url::Host::Ipv6(ip) => ip.into(),
                        url::Host::Domain(d) => {
                            debug!(%location, "UPnP location {d} is not an IP address");
                            return None;
                        }
                    };
                    Some(UpnpDiscoverResponse {
                        received_from: SocketAddr::new(ip, location.port_or_known_default()?),
                        location: location.clone(),
                    })
                })
                .collect();
        }
        let (tx, mut rx) = unbounded_channel();
        let searches = igd::WAN_SERVICE_TYPES.map(|st| {
            discover_once(
                &tx,
                st,
                self.opts.discover_timeout,
                self.bind_device.as_ref(),
            )
        });
        for r in futures::future::join_all(searches).await {
            if let Err(e) = r {
                warn!("failed to run SSDP discovery: {e:#}");
            }
        }
        drop(tx);
        let mut responses = Vec::new();
        while let Some(r) = rx.recv().await {
            responses.push(r);
        }
        responses
    }

    async fn upnp_device(
        &self,
        client: &Client,
        response: UpnpDiscoverResponse,
    ) -> anyhow::Result<Option<UpnpEvent>> {
        let desc: RootDesc = fetch_root_desc(client, &response.location).await?;
        let services: Vec<IgdService> = desc
            .devices
            .iter()
            .flat_map(|d| d.iter_services(tracing::Span::none()))
            .filter(|(_, s)| igd::WAN_SERVICE_TYPES.contains(&s.service_type.as_str()))
            .filter_map(|(_, s)| match response.location.join(&s.control_url) {
                Ok(control_url) => Some(IgdService {
                    control_url,
                    service_type: s.service_type.clone(),
                }),
                Err(e) => {
                    debug!(url = s.control_url, "bad control URL: {e:#}");
                    None
                }
            })
            .collect();
        if services.is_empty() {
            return Ok(None);
        }
        let nics = NetworkInterface::show()?;
        let local_ip = get_local_ip_relative_to(response.received_from, &nics)?;
        if let IpAddr::V4(host) = response.received_from.ip() {
            let mut state = self.lock();
            if !state.igd_hosts.contains(&host) {
                state.igd_hosts.push(host);
            }
        }
        Ok(Some(UpnpEvent::Device { services, local_ip }))
    }

    /// `AddPortMapping`, retried with a permanent lease when the router only takes those.
    async fn upnp_add(
        &self,
        client: &Client,
        service: &IgdService,
        key: PortKey,
        local_ip: IpAddr,
        lease: &mut u32,
    ) -> Result<(), SoapError> {
        let (protocol, port) = key;
        let description = &self.opts.description;
        match service
            .add_port_mapping(client, protocol, port, local_ip, description, *lease)
            .await
        {
            Err(SoapError::Upnp { code, .. })
                if code == igd::ERROR_ONLY_PERMANENT_LEASES_SUPPORTED && *lease != 0 =>
            {
                debug!("router only supports permanent leases");
                *lease = 0;
                service
                    .add_port_mapping(client, protocol, port, local_ip, description, 0)
                    .await
            }
            r => r,
        }
    }

    /// Keeps the ports mapped on one UPnP service. Returns its control URL when the service
    /// stops answering, so discovery can find it (or its replacement) again.
    async fn run_upnp_service(
        &self,
        client: &Client,
        service: IgdService,
        local_ip: IpAddr,
    ) -> Url {
        let source = Source::Upnp(service.control_url.clone());
        for key in &self.ports {
            self.set_outcome(*key, &source, PortMappingMethod::Upnp, Outcome::Pending);
        }
        let mut lease = self.requested_lifetime();
        loop {
            // Only the mapping requests decide whether the service works: some routers answer
            // `GetExternalIPAddress` badly and map ports all the same.
            let external_ip = match service.get_external_ip(client).await {
                Ok(ip) => ip,
                Err(e) => {
                    debug!("GetExternalIPAddress failed: {e}");
                    None
                }
            };
            let mut any_failed = false;
            for key in &self.ports {
                let (protocol, port) = *key;
                let outcome = match self
                    .upnp_add(client, &service, *key, local_ip, &mut lease)
                    .await
                {
                    Ok(()) => {
                        let mut state = self.lock();
                        if !state
                            .upnp_mapped
                            .iter()
                            .any(|(s, k)| s == &service && k == key)
                        {
                            state.upnp_mapped.push((service.clone(), *key));
                        }
                        Outcome::Mapped {
                            external_port: port,
                            external_ip,
                        }
                    }
                    Err(SoapError::Upnp { code, .. })
                        if code == igd::ERROR_CONFLICT_IN_MAPPING_ENTRY =>
                    {
                        any_failed = true;
                        Outcome::Failed(format!(
                            "UPnP {}: external {protocol:?} port {port} is mapped to another host \
                             (error 718 ConflictInMappingEntry)",
                            service.control_url
                        ))
                    }
                    Err(e @ SoapError::Upnp { .. }) => {
                        any_failed = true;
                        Outcome::Failed(format!("UPnP {}: {e}", service.control_url))
                    }
                    Err(SoapError::Transport(e)) => {
                        debug!(?protocol, port, "AddPortMapping failed: {e:#}");
                        return service.control_url;
                    }
                };
                self.set_outcome(*key, &source, PortMappingMethod::Upnp, outcome);
            }
            // A permanent lease does not run out, but routers forget them on reboot: refresh
            // them as if they had the lease we asked for.
            let renew = self.renew_after(if lease == 0 {
                self.requested_lifetime()
            } else {
                lease
            });
            let wait = if any_failed {
                renew.min(self.opts.discover_interval)
            } else {
                renew
            };
            tokio::time::sleep(wait).await;
        }
    }

    // ---------------------------------------------------------------------------------------
    // NAT-PMP / PCP

    fn gateway_candidates(&self) -> Vec<Ipv4Addr> {
        let interfaces = match NetworkInterface::show() {
            Ok(i) => i,
            Err(e) => {
                debug!("error listing network interfaces: {e:#}");
                Vec::new()
            }
        };
        let igd_hosts = self.lock().igd_hosts.clone();
        gateway::gateway_candidates(
            &interfaces,
            &gateway::default_routes(),
            &igd_hosts,
            self.bind_device.as_ref().map(|d| d.index().get()),
        )
    }

    async fn run_gateways(&self) -> ! {
        let opts = &self.opts;
        loop {
            let (found, candidates) = if opts.gateways.is_empty() {
                let candidates = self.gateway_candidates();
                debug!(?candidates, "probing NAT-PMP / PCP gateway candidates");
                let found = gateway::probe(
                    &candidates,
                    opts.gateway_port,
                    self.retransmit(),
                    opts.discover_timeout,
                    self.bind_device.as_ref(),
                )
                .await;
                (found, Some(candidates))
            } else {
                let given = opts
                    .gateways
                    .iter()
                    .map(|addr| gateway::FoundGateway {
                        addr: *addr,
                        external_ip: None,
                    })
                    .collect();
                (given, None)
            };

            if found.is_empty() {
                let asked = candidates.as_deref().unwrap_or_default();
                let why = if asked.is_empty() {
                    "NAT-PMP/PCP: no gateway candidates".to_owned()
                } else {
                    format!("NAT-PMP/PCP: no gateway answered (asked {asked:?})")
                };
                debug!("{why}");
                self.lock().gateways = Discovery::NotFound(why);
                tokio::time::sleep(opts.discover_interval).await;
                continue;
            }
            self.lock().gateways = Discovery::Found;

            let mut runs: FuturesUnordered<_> = found
                .iter()
                .map(|g| {
                    let span = debug_span!("gateway", addr = %g.addr);
                    self.run_gateway(*g).instrument(span)
                })
                .collect();
            let mut network_check = tokio::time::interval_at(
                Instant::now() + opts.discover_interval,
                opts.discover_interval,
            );
            network_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Serve the gateways until one stops answering or the network changes, then look
            // for gateways again.
            let end = loop {
                tokio::select! {
                    Some(end) = runs.next() => break end,
                    _ = network_check.tick(), if candidates.is_some() => {
                        let now = self.gateway_candidates();
                        if Some(&now) != candidates.as_ref() {
                            debug!(?now, "gateway candidates changed, discovering again");
                            break GatewayEnd { had_success: true, why: String::new() };
                        }
                    }
                }
            };
            drop(runs);
            self.remove_sources(|s| matches!(s, Source::Gateway(_)));
            if end.had_success {
                self.lock().gateways = Discovery::Searching;
            } else {
                debug!("{}", end.why);
                self.lock().gateways = Discovery::NotFound(end.why);
                tokio::time::sleep(opts.discover_interval).await;
            }
        }
    }

    async fn run_gateway(&self, found: gateway::FoundGateway) -> GatewayEnd {
        let target = SocketAddr::new(found.addr, self.opts.gateway_port);
        let socket = match GatewaySocket::connect(target, self.bind_device.as_ref()).await {
            Ok(s) => s,
            Err(e) => {
                return GatewayEnd {
                    had_success: false,
                    why: format!("NAT-PMP/PCP {target}: {e:#}"),
                };
            }
        };
        let slots = {
            let mut state = self.lock();
            let memory = state
                .gateway_memory
                .entry(target)
                .or_insert_with(|| GatewayMemory {
                    pcp: true,
                    mapped: BTreeSet::new(),
                    nonces: BTreeMap::new(),
                });
            let now = Instant::now();
            self.ports
                .iter()
                .map(|key| Slot {
                    key: *key,
                    nonce: *memory.nonces.entry(*key).or_insert_with(random_nonce),
                    granted: None,
                    next: now,
                })
                .collect()
        };
        let mut run = GatewayRun {
            inner: self,
            source: Source::Gateway(target),
            socket,
            pcp: true,
            pcp_retry_at: None,
            epoch: EpochTracker::new(self.opts.protocol_second),
            lost_state: false,
            natpmp_external_ip: found.external_ip.map(Some),
            slots,
            had_success: false,
        };
        for key in &self.ports {
            self.set_outcome(*key, &run.source, PortMappingMethod::Pcp, Outcome::Pending);
        }
        let why = run.run().await;
        GatewayEnd {
            had_success: run.had_success,
            why,
        }
    }

    // ---------------------------------------------------------------------------------------
    // Shutdown

    async fn shutdown(&self) {
        let (upnp, gateways) = {
            let mut state = self.lock();
            let upnp = std::mem::take(&mut state.upnp_mapped);
            let gateways: Vec<(SocketAddr, GatewayMemory)> =
                std::mem::take(&mut state.gateway_memory)
                    .into_iter()
                    .filter(|(_, m)| !m.mapped.is_empty())
                    .collect();
            (upnp, gateways)
        };
        let delete_upnp = async {
            if upnp.is_empty() {
                return;
            }
            let client = match self.http_client() {
                Ok(c) => c,
                Err(e) => {
                    debug!("cannot delete UPnP mappings: {e:#}");
                    return;
                }
            };
            let deletions = upnp.iter().map(|(service, (protocol, port))| {
                let client = &client;
                async move {
                    match service.delete_port_mapping(client, *protocol, *port).await {
                        Ok(()) => debug!(url = %service.control_url, ?protocol, port, "UPnP mapping deleted"),
                        Err(e) => debug!(url = %service.control_url, ?protocol, port, "DeletePortMapping failed: {e}"),
                    }
                }
            });
            futures::future::join_all(deletions).await;
        };
        let delete_gateways = futures::future::join_all(
            gateways
                .iter()
                .map(|(addr, memory)| self.delete_gateway_mappings(*addr, memory)),
        );
        let deleting = futures::future::join(delete_upnp, delete_gateways);
        if tokio::time::timeout(self.opts.shutdown_timeout, deleting)
            .await
            .is_err()
        {
            debug!("gave up deleting port mappings");
        }
        let mut state = self.lock();
        for row in state.rows.values_mut() {
            row.clear();
        }
        let state = &mut *state;
        for d in [&mut state.upnp, &mut state.gateways] {
            if *d != Discovery::Disabled {
                *d = Discovery::Stopped;
            }
        }
    }

    /// Deletes our mappings on one gateway, one request at a time (RFC 6886 section 3.1 asks
    /// clients not to send several requests at once).
    async fn delete_gateway_mappings(&self, gateway: SocketAddr, memory: &GatewayMemory) {
        let socket = match GatewaySocket::connect(gateway, self.bind_device.as_ref()).await {
            Ok(s) => s,
            Err(e) => {
                debug!(%gateway, "cannot delete mappings: {e:#}");
                return;
            }
        };
        for (protocol, port) in &memory.mapped {
            let (protocol, port) = (*protocol, *port);
            let result = match memory.nonces.get(&(protocol, port)) {
                Some(nonce) if memory.pcp => {
                    let req = pcp::MapRequest::delete(socket.local_ip(), *nonce, protocol, port);
                    socket
                        .exchange(&req.encode(), self.retransmit(), |buf| {
                            pcp_answer(buf, &req)
                        })
                        .await
                        .map(|_| ())
                }
                _ => {
                    let req = natpmp::Request::Map {
                        protocol,
                        internal_port: port,
                        suggested_external_port: 0,
                        lifetime: 0,
                    };
                    socket
                        .exchange(&req.encode(), self.retransmit(), |buf| {
                            natpmp_map_answer(buf, protocol, port)
                        })
                        .await
                        .map(|_| ())
                }
            };
            match result {
                Ok(()) => debug!(%gateway, ?protocol, port, "mapping deleted"),
                Err(e) => debug!(%gateway, ?protocol, port, "error deleting mapping: {e}"),
            }
        }
    }
}

/// One port on one NAT-PMP / PCP gateway.
struct Slot {
    key: PortKey,
    nonce: pcp::Nonce,
    /// What the gateway granted last: renewals ask for the same.
    granted: Option<(u16, Option<IpAddr>)>,
    next: Instant,
}

/// Serves one NAT-PMP / PCP gateway: maps the ports, renews them, recreates them when the
/// gateway lost its state.
struct GatewayRun<'a> {
    inner: &'a Inner,
    source: Source,
    socket: GatewaySocket,
    /// Speaking PCP (else NAT-PMP).
    pcp: bool,
    /// When to give PCP another chance after falling back to NAT-PMP.
    pcp_retry_at: Option<Instant>,
    epoch: EpochTracker,
    /// Set when an answer showed that the gateway lost its mappings.
    lost_state: bool,
    /// The NAT-PMP public address: `None` until asked, `Some(None)` when the gateway had none.
    natpmp_external_ip: Option<Option<Ipv4Addr>>,
    slots: Vec<Slot>,
    had_success: bool,
}

impl GatewayRun<'_> {
    fn method(&self) -> PortMappingMethod {
        if self.pcp {
            PortMappingMethod::Pcp
        } else {
            PortMappingMethod::NatPmp
        }
    }

    fn gateway(&self) -> SocketAddr {
        self.socket.gateway()
    }

    fn set_protocol(&mut self, pcp: bool) {
        if self.pcp == pcp {
            return;
        }
        debug!(pcp, "switching protocol");
        self.pcp = pcp;
        // The two protocols are separate servers as far as epochs go.
        self.epoch.reset();
        self.pcp_retry_at = (!pcp).then(|| Instant::now() + self.inner.router_secs(PCP_RETRY_SECS));
        if let Some(m) = self.inner.lock().gateway_memory.get_mut(&self.gateway()) {
            m.pcp = pcp;
        }
    }

    fn observe_epoch(&mut self, rule: EpochRule, epoch: u32) {
        if self.epoch.observe(rule, Instant::now(), epoch) {
            info!(gateway = %self.gateway(), "gateway lost its mappings, recreating them");
            self.lost_state = true;
        }
    }

    /// Returns why the gateway is no longer served.
    async fn run(&mut self) -> String {
        loop {
            let now = Instant::now();
            for i in 0..self.slots.len() {
                if self.slots.get(i).is_none_or(|s| s.next > now) {
                    continue;
                }
                if let Err(why) = self.map_slot(i).await {
                    return why;
                }
                if std::mem::take(&mut self.lost_state) {
                    // The slot just answered is fresh; everything else is gone on the gateway.
                    let now = Instant::now();
                    for (j, slot) in self.slots.iter_mut().enumerate() {
                        if j != i {
                            slot.next = now;
                        }
                    }
                    if !self.pcp {
                        self.natpmp_external_ip = None;
                    }
                }
            }
            match self.slots.iter().map(|s| s.next).min() {
                Some(next) => tokio::time::sleep_until(next).await,
                None => return "nothing to map".to_owned(),
            }
        }
    }

    async fn map_slot(&mut self, i: usize) -> Result<(), String> {
        let lifetime = self.inner.requested_lifetime();
        if !self.pcp && self.pcp_retry_at.is_some_and(|t| t <= Instant::now()) {
            self.set_protocol(true);
        }
        let granted = if self.pcp {
            match self.pcp_map(i, lifetime).await {
                Ok(g) => g,
                Err(failure) => {
                    let unsupported = matches!(failure, PcpFailure::Unsupported);
                    let why = match failure {
                        PcpFailure::Unsupported => "the gateway does not speak PCP".to_owned(),
                        PcpFailure::Silent(e) => format!("no PCP answer: {e}"),
                    };
                    if self.gateway().is_ipv4() {
                        // NAT-PMP is IPv4 only. A silent gateway may just ignore PCP.
                        debug!("{why}, trying NAT-PMP");
                        self.set_protocol(false);
                        self.natpmp_map(i, lifetime).await?
                    } else if unsupported {
                        Granted::Refused {
                            why: format!("PCP {}: {why}", self.gateway()),
                            retry_after: self.inner.router_secs(PCP_RETRY_SECS),
                        }
                    } else {
                        return Err(format!("PCP {}: {why}", self.gateway()));
                    }
                }
            }
        } else {
            self.natpmp_map(i, lifetime).await?
        };

        let method = self.method();
        let source = self.source.clone();
        let gateway = self.gateway();
        let Some(slot) = self.slots.get_mut(i) else {
            return Ok(());
        };
        let now = Instant::now();
        let outcome = match granted {
            Granted::Mapped {
                external_port,
                external_ip,
                lifetime,
            } => {
                slot.granted = Some((external_port, external_ip));
                slot.next = now + self.inner.renew_after(lifetime);
                if let Some(m) = self.inner.lock().gateway_memory.get_mut(&gateway) {
                    m.mapped.insert(slot.key);
                }
                self.had_success = true;
                Outcome::Mapped {
                    external_port,
                    external_ip,
                }
            }
            Granted::Refused { why, retry_after } => {
                slot.granted = None;
                slot.next = now + retry_after;
                Outcome::Failed(why)
            }
        };
        self.inner.set_outcome(slot.key, &source, method, outcome);
        Ok(())
    }

    async fn pcp_map(&mut self, i: usize, lifetime: u32) -> Result<Granted, PcpFailure> {
        let Some(slot) = self.slots.get(i) else {
            return Err(PcpFailure::Unsupported);
        };
        let (protocol, internal_port) = slot.key;
        let req = pcp::MapRequest {
            lifetime,
            client_ip: self.socket.local_ip(),
            nonce: slot.nonce,
            protocol,
            internal_port,
            suggested_external_port: slot.granted.map_or(internal_port, |(port, _)| port),
            suggested_external_ip: slot.granted.and_then(|(_, ip)| ip),
        };
        let answer = self
            .socket
            .exchange(&req.encode(), self.inner.retransmit(), |buf| {
                pcp_answer(buf, &req)
            })
            .await
            .map_err(PcpFailure::Silent)?;
        let r = match answer {
            PcpAnswer::Unsupported => return Err(PcpFailure::Unsupported),
            PcpAnswer::Response(r) => r,
        };
        self.observe_epoch(EpochRule::Pcp, r.epoch);
        let gateway = self.gateway();
        Ok(match (r.result, r.map) {
            (pcp::ResultCode::Success, Some(map)) if r.lifetime > 0 => Granted::Mapped {
                external_port: map.assigned_external_port,
                external_ip: Some(map.assigned_external_ip),
                lifetime: r.lifetime,
            },
            (pcp::ResultCode::Success, _) => Granted::Refused {
                why: format!("PCP {gateway}: granted no lifetime"),
                retry_after: self.inner.opts.discover_interval,
            },
            (result, _) => Granted::Refused {
                why: format!("PCP {gateway}: {result}"),
                // Section 8.3: do not ask again while the error lasts, within reason.
                retry_after: self
                    .inner
                    .router_secs(r.lifetime.min(PCP_RETRY_SECS))
                    .max(self.inner.opts.discover_interval),
            },
        })
    }

    async fn natpmp_map(&mut self, i: usize, lifetime: u32) -> Result<Granted, String> {
        let gateway = self.gateway();
        let silent = |e: ExchangeError| format!("NAT-PMP {gateway}: {e}");
        if self.natpmp_external_ip.is_none() {
            let r = self
                .socket
                .exchange(
                    &natpmp::Request::PublicAddress.encode(),
                    self.inner.retransmit(),
                    natpmp_public_answer,
                )
                .await
                .map_err(silent)?;
            self.observe_epoch(EpochRule::NatPmp, r.epoch);
            self.natpmp_external_ip = Some(match r.body {
                natpmp::ResponseBody::PublicAddress(ip)
                    if r.result == natpmp::ResultCode::Success =>
                {
                    Some(ip)
                }
                _ => {
                    debug!(result = %r.result, "no public address");
                    None
                }
            });
        }
        let Some(slot) = self.slots.get(i) else {
            return Err(format!("NAT-PMP {gateway}: no such port"));
        };
        let (protocol, internal_port) = slot.key;
        let req = natpmp::Request::Map {
            protocol,
            internal_port,
            suggested_external_port: slot.granted.map_or(internal_port, |(port, _)| port),
            lifetime,
        };
        let r = self
            .socket
            .exchange(&req.encode(), self.inner.retransmit(), |buf| {
                natpmp_map_answer(buf, protocol, internal_port)
            })
            .await
            .map_err(silent)?;
        self.observe_epoch(EpochRule::NatPmp, r.epoch);
        Ok(match (r.result, r.body) {
            (
                natpmp::ResultCode::Success,
                natpmp::ResponseBody::Map {
                    external_port,
                    lifetime,
                    ..
                },
            ) if lifetime > 0 => Granted::Mapped {
                external_port,
                external_ip: self.natpmp_external_ip.flatten().map(IpAddr::V4),
                lifetime,
            },
            (natpmp::ResultCode::Success, _) => Granted::Refused {
                why: format!("NAT-PMP {gateway}: granted no lifetime"),
                retry_after: self.inner.opts.discover_interval,
            },
            (result, _) => Granted::Refused {
                why: format!("NAT-PMP {gateway}: {result}"),
                retry_after: self.inner.opts.discover_interval,
            },
        })
    }
}
