//! Fakes of the routers the port mapper talks to, for the tests of this crate and of crates that
//! use [`PortMapper`](crate::PortMapper): a NAT-PMP / PCP gateway on a loopback UDP socket
//! ([`FakeGateway`]) and a UPnP internet gateway device served over loopback HTTP ([`FakeIgd`]).
//! Point `PortMappingOptions::gateways` + `gateway_port` and `PortMappingOptions::upnp_locations`
//! at them.
//!
//! Both record what they were asked and keep a mapping table that tests can read. They are
//! simple, but strict where the mapper must be right: PCP mappings belong to the nonce that made
//! them, mappings expire when their lifetime runs out, and the epoch restarts on
//! [`FakeGateway::reset_epoch`].

use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    task::{JoinHandle, JoinSet},
    time::Instant,
};
use tracing::trace;
use url::Url;

use crate::{PortMappingMethod, PortMappingProtocol, igd, natpmp, pcp};

type Key = (PortMappingProtocol, u16);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Which protocols a [`FakeGateway`] speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeGatewayProtocols {
    /// PCP and NAT-PMP, like most current routers.
    Both,
    /// NAT-PMP only: PCP requests get a NAT-PMP `UNSUPP_VERSION` answer (RFC 6886 section 3.5).
    NatPmpOnly,
    /// PCP only: NAT-PMP requests get a PCP `UNSUPP_VERSION` answer (RFC 6887 section 9).
    PcpOnly,
}

/// An error a [`FakeGateway`] answers the mapping requests of one port with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeRefusal {
    /// PCP `NOT_AUTHORIZED` / NAT-PMP "not authorized / refused".
    NotAuthorized,
    /// PCP `NO_RESOURCES` / NAT-PMP "out of resources".
    OutOfResources,
}

impl FakeRefusal {
    fn pcp(self) -> pcp::ResultCode {
        match self {
            FakeRefusal::NotAuthorized => pcp::ResultCode::NotAuthorized,
            FakeRefusal::OutOfResources => pcp::ResultCode::NoResources,
        }
    }

    fn natpmp(self) -> natpmp::ResultCode {
        match self {
            FakeRefusal::NotAuthorized => natpmp::ResultCode::NotAuthorized,
            FakeRefusal::OutOfResources => natpmp::ResultCode::OutOfResources,
        }
    }
}

/// Settings of a [`FakeGateway`].
#[derive(Debug, Clone)]
pub struct FakeGatewayOptions {
    pub protocols: FakeGatewayProtocols,
    /// The public address the gateway reports and maps on.
    pub external_ip: Ipv4Addr,
    /// The longest lifetime granted, in gateway seconds; longer requests are cut to it.
    pub max_lifetime: u32,
    /// How long one gateway second lasts, for lifetimes and the epoch. Tests shrink it and give
    /// the mapper the same value as `PortMappingOptions::protocol_second`.
    pub second: Duration,
}

impl Default for FakeGatewayOptions {
    fn default() -> Self {
        Self {
            protocols: FakeGatewayProtocols::Both,
            external_ip: Ipv4Addr::new(203, 0, 113, 7),
            max_lifetime: 7200,
            second: Duration::from_secs(1),
        }
    }
}

/// A request a [`FakeGateway`] received, dropped ones included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeGatewayRequest {
    NatPmpPublicAddress,
    NatPmpMap {
        protocol: PortMappingProtocol,
        internal_port: u16,
        suggested_external_port: u16,
        lifetime: u32,
    },
    PcpMap {
        protocol: PortMappingProtocol,
        internal_port: u16,
        suggested_external_port: u16,
        lifetime: u32,
        nonce: [u8; 12],
    },
}

impl FakeGatewayRequest {
    /// The port a mapping request is about; `None` for the public address request.
    pub fn port(&self) -> Option<(PortMappingProtocol, u16)> {
        match self {
            FakeGatewayRequest::NatPmpPublicAddress => None,
            FakeGatewayRequest::NatPmpMap {
                protocol,
                internal_port,
                ..
            }
            | FakeGatewayRequest::PcpMap {
                protocol,
                internal_port,
                ..
            } => Some((*protocol, *internal_port)),
        }
    }

    /// The lifetime a mapping request asks for (0 deletes); `None` for the public address
    /// request.
    pub fn lifetime(&self) -> Option<u32> {
        match self {
            FakeGatewayRequest::NatPmpPublicAddress => None,
            FakeGatewayRequest::NatPmpMap { lifetime, .. }
            | FakeGatewayRequest::PcpMap { lifetime, .. } => Some(*lifetime),
        }
    }
}

/// A live mapping on a [`FakeGateway`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeGatewayMapping {
    pub protocol: PortMappingProtocol,
    pub internal_port: u16,
    pub external_port: u16,
    pub method: PortMappingMethod,
    /// The lifetime granted, in gateway seconds.
    pub lifetime: u32,
}

#[derive(Debug)]
struct LiveMapping {
    external_port: u16,
    method: PortMappingMethod,
    lifetime: u32,
    expires: Instant,
    /// The PCP nonce that owns the mapping; `None` when NAT-PMP made it.
    nonce: Option<pcp::Nonce>,
}

/// So that a misbehaving client cannot grow the log without end.
const MAX_LOGGED_REQUESTS: usize = 100_000;

#[derive(Debug)]
struct GatewayState {
    opts: FakeGatewayOptions,
    epoch_start: Instant,
    epoch_base: u32,
    silent: bool,
    drop_next: usize,
    external_ports: BTreeMap<Key, u16>,
    refusals: BTreeMap<Key, FakeRefusal>,
    mappings: BTreeMap<Key, LiveMapping>,
    requests: Vec<FakeGatewayRequest>,
}

impl GatewayState {
    fn epoch(&self, now: Instant) -> u32 {
        let secs = now.saturating_duration_since(self.epoch_start).as_nanos()
            / self.opts.second.as_nanos().max(1);
        self.epoch_base
            .saturating_add(u32::try_from(secs).unwrap_or(u32::MAX))
    }

    fn expire(&mut self, now: Instant) {
        self.mappings.retain(|_, m| m.expires > now);
    }

    fn log(&mut self, request: FakeGatewayRequest) {
        if self.requests.len() < MAX_LOGGED_REQUESTS {
            self.requests.push(request);
        }
    }

    /// Whether to throw this datagram away instead of answering it.
    fn discard(&mut self) -> bool {
        if self.silent {
            return true;
        }
        if self.drop_next > 0 {
            self.drop_next -= 1;
            return true;
        }
        false
    }

    fn grant(
        &mut self,
        key: Key,
        method: PortMappingMethod,
        requested: u32,
        nonce: Option<pcp::Nonce>,
        now: Instant,
    ) -> (u16, u32) {
        let lifetime = requested.min(self.opts.max_lifetime);
        let external_port = self.external_ports.get(&key).copied().unwrap_or(key.1);
        self.mappings.insert(
            key,
            LiveMapping {
                external_port,
                method,
                lifetime,
                expires: now + self.opts.second.saturating_mul(lifetime),
                nonce,
            },
        );
        (external_port, lifetime)
    }

    fn handle(&mut self, buf: &[u8]) -> Option<Vec<u8>> {
        let now = Instant::now();
        self.expire(now);
        let epoch = self.epoch(now);
        match *buf.first()? {
            natpmp::VERSION => self.handle_natpmp(buf, epoch, now),
            pcp::VERSION => self.handle_pcp(buf, epoch, now),
            _ => None,
        }
    }

    fn handle_natpmp(&mut self, buf: &[u8], epoch: u32, now: Instant) -> Option<Vec<u8>> {
        let req = natpmp::Request::decode(buf).ok()?;
        self.log(match req {
            natpmp::Request::PublicAddress => FakeGatewayRequest::NatPmpPublicAddress,
            natpmp::Request::Map {
                protocol,
                internal_port,
                suggested_external_port,
                lifetime,
            } => FakeGatewayRequest::NatPmpMap {
                protocol,
                internal_port,
                suggested_external_port,
                lifetime,
            },
        });
        if self.discard() {
            return None;
        }
        if self.opts.protocols == FakeGatewayProtocols::PcpOnly {
            // A PCP server reads the zero first octet as version 0 and names the version it
            // speaks instead.
            let answer = pcp::Response {
                version: pcp::VERSION,
                opcode: req.opcode(),
                result: pcp::ResultCode::UnsuppVersion,
                lifetime: 0,
                epoch,
                map: None,
            };
            return Some(answer.encode());
        }
        let response = match req {
            natpmp::Request::PublicAddress => natpmp::Response {
                opcode: natpmp::OP_PUBLIC_ADDRESS,
                result: natpmp::ResultCode::Success,
                epoch,
                body: natpmp::ResponseBody::PublicAddress(self.opts.external_ip),
            },
            natpmp::Request::Map {
                protocol,
                internal_port,
                lifetime,
                ..
            } => {
                let key = (protocol, internal_port);
                let (result, external_port, lifetime) = if lifetime == 0 {
                    self.mappings.remove(&key);
                    (natpmp::ResultCode::Success, 0, 0)
                } else if let Some(refusal) = self.refusals.get(&key) {
                    (refusal.natpmp(), 0, 0)
                } else {
                    let (port, lifetime) =
                        self.grant(key, PortMappingMethod::NatPmp, lifetime, None, now);
                    (natpmp::ResultCode::Success, port, lifetime)
                };
                natpmp::Response {
                    opcode: req.opcode(),
                    result,
                    epoch,
                    body: natpmp::ResponseBody::Map {
                        internal_port,
                        external_port,
                        lifetime,
                    },
                }
            }
        };
        Some(response.encode())
    }

    fn handle_pcp(&mut self, buf: &[u8], epoch: u32, now: Instant) -> Option<Vec<u8>> {
        let req = pcp::MapRequest::decode(buf).ok()?;
        self.log(FakeGatewayRequest::PcpMap {
            protocol: req.protocol,
            internal_port: req.internal_port,
            suggested_external_port: req.suggested_external_port,
            lifetime: req.lifetime,
            nonce: req.nonce,
        });
        if self.discard() {
            return None;
        }
        if self.opts.protocols == FakeGatewayProtocols::NatPmpOnly {
            let answer = natpmp::Response {
                opcode: pcp::OP_MAP,
                result: natpmp::ResultCode::UnsupportedVersion,
                epoch,
                body: natpmp::ResponseBody::None,
            };
            return Some(answer.encode());
        }
        let key = (req.protocol, req.internal_port);
        // An IPv6 client gets a firewall pinhole: its own address and port, no translation.
        let pinhole = matches!(req.client_ip, IpAddr::V6(_));
        let external_ip = if pinhole {
            req.client_ip
        } else {
            IpAddr::V4(self.opts.external_ip)
        };
        let owned_by_other = self
            .mappings
            .get(&key)
            .and_then(|m| m.nonce)
            .is_some_and(|n| n != req.nonce);
        let (result, lifetime, external_port) = if owned_by_other {
            // RFC 6887 section 11.3: a mapping only answers to the nonce that created it.
            (pcp::ResultCode::NotAuthorized, 30, 0)
        } else if req.lifetime == 0 {
            self.mappings.remove(&key);
            (pcp::ResultCode::Success, 0, 0)
        } else if let Some(refusal) = self.refusals.get(&key) {
            (refusal.pcp(), 30, 0)
        } else {
            let method = PortMappingMethod::Pcp;
            let (port, lifetime) = if pinhole {
                let lifetime = req.lifetime.min(self.opts.max_lifetime);
                self.mappings.insert(
                    key,
                    LiveMapping {
                        external_port: req.internal_port,
                        method,
                        lifetime,
                        expires: now + self.opts.second.saturating_mul(lifetime),
                        nonce: Some(req.nonce),
                    },
                );
                (req.internal_port, lifetime)
            } else {
                self.grant(key, method, req.lifetime, Some(req.nonce), now)
            };
            (pcp::ResultCode::Success, lifetime, port)
        };
        let answer = pcp::Response {
            version: pcp::VERSION,
            opcode: pcp::OP_MAP,
            result,
            lifetime,
            epoch,
            map: Some(pcp::MapResponseData {
                nonce: req.nonce,
                protocol: req.protocol,
                internal_port: req.internal_port,
                assigned_external_port: external_port,
                assigned_external_ip: external_ip,
            }),
        };
        Some(answer.encode())
    }
}

/// A NAT-PMP / PCP gateway on a UDP socket. Stops when dropped.
#[derive(Debug)]
pub struct FakeGateway {
    addr: SocketAddr,
    state: Arc<Mutex<GatewayState>>,
    task: JoinHandle<()>,
}

impl FakeGateway {
    /// Starts a gateway on a free UDP port of `ip` (a loopback address in tests).
    pub async fn start(ip: IpAddr, opts: FakeGatewayOptions) -> std::io::Result<Self> {
        let socket = UdpSocket::bind((ip, 0)).await?;
        let addr = socket.local_addr()?;
        let state = Arc::new(Mutex::new(GatewayState {
            opts,
            epoch_start: Instant::now(),
            // Far enough from 0 that a reset is a clear step back.
            epoch_base: 10_000,
            silent: false,
            drop_next: 0,
            external_ports: BTreeMap::new(),
            refusals: BTreeMap::new(),
            mappings: BTreeMap::new(),
            requests: Vec::new(),
        }));
        let task = tokio::spawn(serve_gateway(socket, state.clone()));
        Ok(Self { addr, state, task })
    }

    /// The address and port the gateway listens on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Every request received so far, in order.
    pub fn requests(&self) -> Vec<FakeGatewayRequest> {
        lock(&self.state).requests.clone()
    }

    /// The mappings that are live now.
    pub fn mappings(&self) -> Vec<FakeGatewayMapping> {
        let mut state = lock(&self.state);
        state.expire(Instant::now());
        state
            .mappings
            .iter()
            .map(|((protocol, internal_port), m)| FakeGatewayMapping {
                protocol: *protocol,
                internal_port: *internal_port,
                external_port: m.external_port,
                method: m.method,
                lifetime: m.lifetime,
            })
            .collect()
    }

    /// Grants `external_port` (instead of the internal port) to mappings of this port.
    pub fn set_external_port(
        &self,
        protocol: PortMappingProtocol,
        internal_port: u16,
        external_port: u16,
    ) {
        lock(&self.state)
            .external_ports
            .insert((protocol, internal_port), external_port);
    }

    /// Answers mapping requests for this port with an error from now on.
    pub fn refuse(&self, protocol: PortMappingProtocol, internal_port: u16, refusal: FakeRefusal) {
        lock(&self.state)
            .refusals
            .insert((protocol, internal_port), refusal);
    }

    /// Drops the next `n` datagrams without answering (they are still logged).
    pub fn drop_next(&self, n: usize) {
        lock(&self.state).drop_next = n;
    }

    /// Stops (or resumes) answering anything.
    pub fn set_silent(&self, silent: bool) {
        lock(&self.state).silent = silent;
    }

    /// Simulates a reboot: the mapping table is lost and the epoch starts again from 0.
    pub fn reset_epoch(&self) {
        let mut state = lock(&self.state);
        state.mappings.clear();
        state.epoch_base = 0;
        state.epoch_start = Instant::now();
    }
}

impl Drop for FakeGateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_gateway(socket: UdpSocket, state: Arc<Mutex<GatewayState>>) {
    // Bigger than any valid message, so oversized ones reach the decoders and get rejected.
    let mut buf = [0u8; pcp::MAX_MESSAGE_LEN + 100];
    loop {
        let (len, from) = match socket.recv_from(&mut buf).await {
            Ok(r) => r,
            Err(e) => {
                // Windows reports ICMP errors of earlier sends here; nothing to do about them.
                trace!("fake gateway: recv error: {e}");
                continue;
            }
        };
        let answer = lock(&state).handle(buf.get(..len).unwrap_or_default());
        if let Some(answer) = answer
            && let Err(e) = socket.send_to(&answer, from).await
        {
            trace!("fake gateway: error answering {from}: {e}");
        }
    }
}

// -------------------------------------------------------------------------------------------
// UPnP

/// Settings of a [`FakeIgd`].
#[derive(Debug, Clone)]
pub struct FakeIgdOptions {
    /// The type of the one WAN connection service the device has.
    pub service_type: String,
    /// What `GetExternalIPAddress` answers; `None` answers an empty address.
    pub external_ip: Option<IpAddr>,
    /// Refuse leases other than 0 with error 725 `OnlyPermanentLeasesSupported`.
    pub only_permanent_leases: bool,
}

impl Default for FakeIgdOptions {
    fn default() -> Self {
        Self {
            service_type: igd::SERVICE_TYPE_WAN_IP_CONNECTION_1.to_owned(),
            external_ip: Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7))),
            only_permanent_leases: false,
        }
    }
}

/// A SOAP call a [`FakeIgd`] received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeIgdCall {
    /// The `SOAPAction` header, quotes removed.
    pub soap_action: String,
    /// The action named after the `#` of the header.
    pub action: String,
    /// The arguments of the action that `AddPortMapping`, `DeletePortMapping` and
    /// `GetExternalIPAddress` define, as sent.
    pub args: BTreeMap<String, String>,
    /// The request body as sent.
    pub body: String,
}

/// A port mapping on a [`FakeIgd`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeIgdMapping {
    /// `TCP` or `UDP`, as sent.
    pub protocol: String,
    pub external_port: u16,
    pub internal_port: u16,
    pub internal_client: String,
    pub description: String,
    pub lease: u32,
}

#[derive(Debug)]
struct IgdState {
    opts: FakeIgdOptions,
    calls: Vec<FakeIgdCall>,
    mappings: BTreeMap<(String, u16), FakeIgdMapping>,
    /// External ports mapped to another host, as `(protocol, port)`.
    foreign: BTreeSet<(String, u16)>,
}

const IGD_ARGS: [&str; 9] = [
    "NewRemoteHost",
    "NewExternalPort",
    "NewProtocol",
    "NewInternalPort",
    "NewInternalClient",
    "NewEnabled",
    "NewPortMappingDescription",
    "NewLeaseDuration",
    "NewExternalIPAddress",
];

/// The UPnP error codes the fake answers with.
const ERROR_INVALID_ARGS: u16 = 402;
const ERROR_INVALID_ACTION: u16 = 401;
const ERROR_NO_SUCH_ENTRY: u16 = 714;

impl IgdState {
    /// Answers a SOAP call with `Ok(response arguments)` or `Err((code, description))`.
    fn call(&mut self, soap_action: &str, body: &str) -> Result<String, (u16, &'static str)> {
        let soap_action = soap_action.trim_matches('"');
        let (_, action) = soap_action
            .split_once('#')
            .ok_or((ERROR_INVALID_ACTION, "Invalid Action"))?;
        let args: BTreeMap<String, String> = IGD_ARGS
            .iter()
            .filter_map(|name| Some((name.to_string(), igd::element_text(body, name)?)))
            .collect();
        if self.calls.len() < MAX_LOGGED_REQUESTS {
            self.calls.push(FakeIgdCall {
                soap_action: soap_action.to_owned(),
                action: action.to_owned(),
                args: args.clone(),
                body: body.to_owned(),
            });
        }
        let arg = |name: &str| args.get(name).map(String::as_str).unwrap_or_default();
        let port = |name: &str| {
            arg(name)
                .parse::<u16>()
                .map_err(|_| (ERROR_INVALID_ARGS, "Invalid Args"))
        };
        match action {
            "GetExternalIPAddress" => Ok(format!(
                "<NewExternalIPAddress>{}</NewExternalIPAddress>",
                self.opts
                    .external_ip
                    .map(|ip| ip.to_string())
                    .unwrap_or_default()
            )),
            "AddPortMapping" => {
                let protocol = arg("NewProtocol").to_owned();
                let external_port = port("NewExternalPort")?;
                let lease = arg("NewLeaseDuration")
                    .parse::<u32>()
                    .map_err(|_| (ERROR_INVALID_ARGS, "Invalid Args"))?;
                if self.opts.only_permanent_leases && lease != 0 {
                    return Err((
                        igd::ERROR_ONLY_PERMANENT_LEASES_SUPPORTED,
                        "OnlyPermanentLeasesSupported",
                    ));
                }
                if self.foreign.contains(&(protocol.clone(), external_port)) {
                    return Err((
                        igd::ERROR_CONFLICT_IN_MAPPING_ENTRY,
                        "ConflictInMappingEntry",
                    ));
                }
                let mapping = FakeIgdMapping {
                    protocol: protocol.clone(),
                    external_port,
                    internal_port: port("NewInternalPort")?,
                    internal_client: arg("NewInternalClient").to_owned(),
                    description: arg("NewPortMappingDescription").to_owned(),
                    lease,
                };
                self.mappings.insert((protocol, external_port), mapping);
                Ok(String::new())
            }
            "DeletePortMapping" => {
                let key = (arg("NewProtocol").to_owned(), port("NewExternalPort")?);
                match self.mappings.remove(&key) {
                    Some(_) => Ok(String::new()),
                    None => Err((ERROR_NO_SUCH_ENTRY, "NoSuchEntryInArray")),
                }
            }
            _ => Err((ERROR_INVALID_ACTION, "Invalid Action")),
        }
    }

    fn description(&self) -> String {
        format!(
            "<?xml version=\"1.0\"?>\r\n\
             <root xmlns=\"urn:schemas-upnp-org:device-1-0\">\
             <specVersion><major>1</major><minor>0</minor></specVersion>\
             <device>\
             <deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1</deviceType>\
             <friendlyName>fake internet gateway device</friendlyName>\
             <serviceList><service>\
             <serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType>\
             <serviceId>urn:upnp-org:serviceId:L3Forwarding1</serviceId>\
             <controlURL>/l3f</controlURL><eventSubURL>/l3f-events</eventSubURL>\
             <SCPDURL>/l3f.xml</SCPDURL>\
             </service></serviceList>\
             <deviceList><device>\
             <deviceType>urn:schemas-upnp-org:device:WANDevice:1</deviceType>\
             <deviceList><device>\
             <deviceType>urn:schemas-upnp-org:device:WANConnectionDevice:1</deviceType>\
             <serviceList><service>\
             <serviceType>{}</serviceType>\
             <serviceId>urn:upnp-org:serviceId:WANConnection1</serviceId>\
             <controlURL>/ctl</controlURL><eventSubURL>/events</eventSubURL>\
             <SCPDURL>/scpd.xml</SCPDURL>\
             </service></serviceList>\
             </device></deviceList>\
             </device></deviceList>\
             </device>\
             </root>\r\n",
            self.opts.service_type
        )
    }
}

/// A UPnP internet gateway device with one WAN connection service, served over HTTP. Stops when
/// dropped.
#[derive(Debug)]
pub struct FakeIgd {
    location: Url,
    state: Arc<Mutex<IgdState>>,
    task: JoinHandle<()>,
}

impl FakeIgd {
    /// Starts the device on a free TCP port of 127.0.0.1.
    pub async fn start(opts: FakeIgdOptions) -> std::io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = listener.local_addr()?;
        let location = Url::parse(&format!("http://{addr}/rootDesc.xml"))
            .map_err(|e| std::io::Error::other(format!("bad location: {e}")))?;
        let state = Arc::new(Mutex::new(IgdState {
            opts,
            calls: Vec::new(),
            mappings: BTreeMap::new(),
            foreign: BTreeSet::new(),
        }));
        let task = tokio::spawn(serve_igd(listener, state.clone()));
        Ok(Self {
            location,
            state,
            task,
        })
    }

    /// The URL of the device description, for `PortMappingOptions::upnp_locations`.
    pub fn location(&self) -> Url {
        self.location.clone()
    }

    /// Every SOAP call received so far, in order.
    pub fn calls(&self) -> Vec<FakeIgdCall> {
        lock(&self.state).calls.clone()
    }

    /// The current port mappings.
    pub fn mappings(&self) -> Vec<FakeIgdMapping> {
        lock(&self.state).mappings.values().cloned().collect()
    }

    /// Pretends another host holds this external port: `AddPortMapping` for it fails with 718
    /// `ConflictInMappingEntry`.
    pub fn add_foreign_mapping(&self, protocol: PortMappingProtocol, external_port: u16) {
        let protocol = match protocol {
            PortMappingProtocol::Tcp => "TCP",
            PortMappingProtocol::Udp => "UDP",
        };
        lock(&self.state)
            .foreign
            .insert((protocol.to_owned(), external_port));
    }
}

impl Drop for FakeIgd {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_igd(listener: TcpListener, state: Arc<Mutex<IgdState>>) {
    // Owning the connection tasks here aborts them when the server task is aborted.
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    connections.spawn(serve_http_connection(stream, state.clone()));
                }
                Err(e) => trace!("fake IGD: accept error: {e}"),
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

/// Requests are small; anything bigger is refused.
const MAX_HTTP_REQUEST: usize = 64 * 1024;

/// Reads one HTTP request: `(method, path, SOAPAction header, body)`.
async fn read_http_request(stream: &mut TcpStream) -> Option<(String, String, String, String)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(chunk.get(..n)?);
        if buf.len() > MAX_HTTP_REQUEST {
            return None;
        }
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut req = httparse::Request::new(&mut headers);
        let httparse::Status::Complete(header_len) = req.parse(&buf).ok()? else {
            continue;
        };
        let header = |name: &str| {
            req.headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case(name))
                .and_then(|h| std::str::from_utf8(h.value).ok())
                .map(str::to_owned)
        };
        let content_length: usize = header("content-length")
            .map(|v| v.trim().parse().ok())
            .unwrap_or(Some(0))?;
        let soap_action = header("soapaction").unwrap_or_default();
        let method = req.method?.to_owned();
        let path = req.path?.to_owned();
        let total = header_len.checked_add(content_length)?;
        if total > MAX_HTTP_REQUEST {
            return None;
        }
        while buf.len() < total {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(chunk.get(..n)?);
        }
        let body = String::from_utf8_lossy(buf.get(header_len..total)?).into_owned();
        return Some((method, path, soap_action, body));
    }
}

fn http_response(status: &str, content_type: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

async fn serve_http_connection(mut stream: TcpStream, state: Arc<Mutex<IgdState>>) {
    let Some((method, path, soap_action, body)) = read_http_request(&mut stream).await else {
        return;
    };
    const XML: &str = "text/xml; charset=\"utf-8\"";
    let response = match (method.as_str(), path.as_str()) {
        ("GET", "/rootDesc.xml") => http_response("200 OK", XML, &lock(&state).description()),
        ("POST", "/ctl") => {
            let mut state = lock(&state);
            let service_type = state.opts.service_type.clone();
            match state.call(&soap_action, &body) {
                Ok(args) => {
                    let action = soap_action
                        .trim_matches('"')
                        .split_once('#')
                        .map(|(_, a)| a.to_owned())
                        .unwrap_or_default();
                    let body = format!(
                        "<?xml version=\"1.0\"?>\r\n\
                         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
                         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
                         <s:Body><u:{action}Response xmlns:u=\"{service_type}\">{args}\
                         </u:{action}Response></s:Body></s:Envelope>\r\n"
                    );
                    http_response("200 OK", XML, &body)
                }
                Err((code, description)) => {
                    let body = format!(
                        "<?xml version=\"1.0\"?>\r\n\
                         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
                         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
                         <s:Body><s:Fault><faultcode>s:Client</faultcode>\
                         <faultstring>UPnPError</faultstring><detail>\
                         <UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\">\
                         <errorCode>{code}</errorCode>\
                         <errorDescription>{description}</errorDescription>\
                         </UPnPError></detail></s:Fault></s:Body></s:Envelope>\r\n"
                    );
                    http_response("500 Internal Server Error", XML, &body)
                }
            }
        }
        _ => http_response("404 Not Found", "text/plain", "not found"),
    };
    if let Err(e) = stream.write_all(&response).await {
        trace!("fake IGD: error writing a response: {e}");
        return;
    }
    if let Err(e) = stream.shutdown().await {
        trace!("fake IGD: error closing a connection: {e}");
    }
}
