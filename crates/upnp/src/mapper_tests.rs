//! [`PortMapper`] against the fakes of [`crate::test_util`]: a NAT-PMP / PCP gateway on a
//! loopback UDP socket and a UPnP internet gateway device on loopback HTTP. Router time runs at
//! 10 ms per second, so lifetimes and renewals take milliseconds.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    time::Duration,
};

use tokio::{sync::oneshot, task::JoinHandle, time::Instant};

use crate::{
    PortMapper, PortMappingMethod, PortMappingMethods, PortMappingOptions, PortMappingProtocol,
    PortMappingState, PortMappingStatus, igd,
    test_util::{
        FakeGateway, FakeGatewayOptions, FakeGatewayProtocols, FakeGatewayRequest, FakeIgd,
        FakeIgdOptions, FakeRefusal,
    },
};

use PortMappingProtocol::{Tcp, Udp};

const SECOND: Duration = Duration::from_millis(10);
const EXTERNAL_IP: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 7);
const DEADLINE: Duration = Duration::from_secs(10);

fn opts() -> PortMappingOptions {
    PortMappingOptions {
        description: "rqbit test".to_owned(),
        // 400 ms with the fast router second: renewed every 200 ms.
        lease_duration: Duration::from_secs(40),
        discover_interval: Duration::from_millis(100),
        discover_timeout: Duration::from_secs(2),
        retransmit_initial: Duration::from_millis(20),
        retransmit_attempts: 4,
        protocol_second: SECOND,
        shutdown_timeout: Duration::from_secs(2),
        ..Default::default()
    }
}

fn gateway_opts(protocols: FakeGatewayProtocols) -> FakeGatewayOptions {
    FakeGatewayOptions {
        protocols,
        external_ip: EXTERNAL_IP,
        second: SECOND,
        ..Default::default()
    }
}

fn natpmp_opts(gateway: &FakeGateway) -> PortMappingOptions {
    PortMappingOptions {
        gateways: vec![gateway.addr().ip()],
        gateway_port: gateway.addr().port(),
        ..opts()
    }
}

const NATPMP: PortMappingMethods = PortMappingMethods {
    upnp: false,
    natpmp: true,
};
const UPNP: PortMappingMethods = PortMappingMethods {
    upnp: true,
    natpmp: false,
};

struct Running {
    mapper: PortMapper,
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

fn start(
    ports: &[(PortMappingProtocol, u16)],
    methods: PortMappingMethods,
    opts: PortMappingOptions,
) -> Running {
    let mapper = PortMapper::new(ports.iter().copied(), methods, opts, None);
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn({
        let mapper = mapper.clone();
        async move {
            mapper
                .run(async {
                    // A dropped sender stops the mapper as well.
                    let _ = stopped.await;
                })
                .await
        }
    });
    Running { mapper, stop, task }
}

impl Running {
    /// Stops the mapper and returns how long it took to finish.
    async fn stop(self) -> Duration {
        let started = Instant::now();
        self.stop.send(()).unwrap();
        self.task.await.unwrap();
        started.elapsed()
    }

    fn row(&self, protocol: PortMappingProtocol, port: u16) -> PortMappingStatus {
        self.mapper
            .status()
            .into_iter()
            .find(|r| r.protocol == protocol && r.internal_port == port)
            .unwrap()
    }
}

/// Polls `cond` until it returns `Ok` or `deadline` passes.
async fn wait_until(deadline: Duration, mut cond: impl FnMut() -> Result<(), String>) {
    let end = Instant::now() + deadline;
    let mut interval = tokio::time::interval(Duration::from_millis(5));
    loop {
        interval.tick().await;
        match cond() {
            Ok(()) => return,
            Err(e) if Instant::now() >= end => panic!("timed out after {deadline:?}: {e}"),
            Err(_) => {}
        }
    }
}

fn expect_mapped(
    row: &PortMappingStatus,
    method: PortMappingMethod,
    external_port: u16,
    external_ip: IpAddr,
) -> Result<(), String> {
    let expected = PortMappingStatus {
        protocol: row.protocol,
        internal_port: row.internal_port,
        external_port: Some(external_port),
        method: Some(method),
        external_ip: Some(external_ip),
        state: PortMappingState::Mapped,
    };
    if *row == expected {
        Ok(())
    } else {
        Err(format!("{row:?}"))
    }
}

async fn wait_mapped(
    running: &Running,
    ports: &[(PortMappingProtocol, u16)],
    method: PortMappingMethod,
    external_ip: IpAddr,
) {
    wait_until(DEADLINE, || {
        ports.iter().try_for_each(|(protocol, port)| {
            expect_mapped(&running.row(*protocol, *port), method, *port, external_ip)
        })
    })
    .await;
}

fn failed_reason(row: &PortMappingStatus) -> Result<String, String> {
    match &row.state {
        PortMappingState::Failed(why) if row.method.is_none() && row.external_port.is_none() => {
            Ok(why.clone())
        }
        _ => Err(format!("{row:?}")),
    }
}

fn gateway_ports(gateway: &FakeGateway) -> Vec<(PortMappingProtocol, u16, u16, PortMappingMethod)> {
    let mut ports: Vec<_> = gateway
        .mappings()
        .into_iter()
        .map(|m| (m.protocol, m.internal_port, m.external_port, m.method))
        .collect();
    ports.sort_by_key(|p| (p.0, p.1));
    ports
}

/// The deletions (lifetime 0) the gateway received, as `(protocol, port, pcp)`.
fn deletions(gateway: &FakeGateway) -> Vec<(PortMappingProtocol, u16, bool)> {
    gateway
        .requests()
        .into_iter()
        .filter(|r| r.lifetime() == Some(0))
        .filter_map(|r| {
            let (protocol, port) = r.port()?;
            Some((
                protocol,
                port,
                matches!(r, FakeGatewayRequest::PcpMap { .. }),
            ))
        })
        .collect()
}

const PORTS: [(PortMappingProtocol, u16); 3] = [(Tcp, 6881), (Udp, 6881), (Udp, 6890)];

#[tokio::test]
async fn pcp_maps_tcp_and_udp_and_deletes_on_shutdown() {
    let gw = FakeGateway::start(
        Ipv4Addr::LOCALHOST.into(),
        gateway_opts(FakeGatewayProtocols::Both),
    )
    .await
    .unwrap();
    let running = start(&PORTS, NATPMP, natpmp_opts(&gw));
    wait_mapped(&running, &PORTS, PortMappingMethod::Pcp, EXTERNAL_IP.into()).await;
    assert_eq!(
        gateway_ports(&gw),
        vec![
            (Tcp, 6881, 6881, PortMappingMethod::Pcp),
            (Udp, 6881, 6881, PortMappingMethod::Pcp),
            (Udp, 6890, 6890, PortMappingMethod::Pcp),
        ]
    );
    // PCP from the start: the gateway speaks it, so NAT-PMP is never used.
    assert!(
        gw.requests()
            .iter()
            .all(|r| matches!(r, FakeGatewayRequest::PcpMap { .. }))
    );
    // Every port has its own nonce.
    let mut nonces: Vec<_> = gw
        .requests()
        .into_iter()
        .filter_map(|r| match r {
            FakeGatewayRequest::PcpMap { nonce, .. } => Some(nonce),
            _ => None,
        })
        .collect();
    nonces.sort();
    nonces.dedup();
    assert_eq!(nonces.len(), 3);

    running.stop().await;
    assert_eq!(gw.mappings(), vec![]);
    let mut deleted = deletions(&gw);
    deleted.sort();
    assert_eq!(
        deleted,
        vec![(Tcp, 6881, true), (Udp, 6881, true), (Udp, 6890, true)]
    );
}

async fn renews_before_expiry(protocols: FakeGatewayProtocols, method: PortMappingMethod) {
    let gw = FakeGateway::start(Ipv4Addr::LOCALHOST.into(), gateway_opts(protocols))
        .await
        .unwrap();
    let ports = [(Tcp, 6881), (Udp, 6881)];
    let running = start(&ports, NATPMP, natpmp_opts(&gw));
    wait_mapped(&running, &ports, method, EXTERNAL_IP.into()).await;

    // The fake drops mappings when their 400 ms run out. Watch them for several lifetimes: they
    // must never disappear, and the gateway must have seen renewals.
    let maps = |gw: &FakeGateway| {
        gw.requests()
            .iter()
            .filter(|r| r.port() == Some((Tcp, 6881)) && r.lifetime() == Some(40))
            .count()
    };
    let before = maps(&gw);
    let watch_until = Instant::now() + SECOND * 40 * 3;
    while Instant::now() < watch_until {
        assert_eq!(gateway_ports(&gw).len(), 2, "a mapping expired");
        assert_eq!(running.row(Tcp, 6881).state, PortMappingState::Mapped);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let renewals = maps(&gw) - before;
    assert!(renewals >= 3, "{renewals} renewals in 3 lifetimes");

    running.stop().await;
    assert_eq!(gw.mappings(), vec![]);
}

#[tokio::test]
async fn pcp_renews_before_expiry() {
    renews_before_expiry(FakeGatewayProtocols::Both, PortMappingMethod::Pcp).await;
}

#[tokio::test]
async fn natpmp_renews_before_expiry() {
    renews_before_expiry(FakeGatewayProtocols::NatPmpOnly, PortMappingMethod::NatPmp).await;
}

async fn epoch_reset_recreates_mappings_at_once(
    protocols: FakeGatewayProtocols,
    method: PortMappingMethod,
) {
    let gw = FakeGateway::start(Ipv4Addr::LOCALHOST.into(), gateway_opts(protocols))
        .await
        .unwrap();
    // One refused port keeps the mapper talking to the gateway (it retries after at most
    // 300 ms); the others are granted 10 s and would only be renewed after 5 s.
    gw.refuse(Udp, 7000, FakeRefusal::OutOfResources);
    let mapped = [(Tcp, 6881), (Udp, 6881)];
    let opts = PortMappingOptions {
        lease_duration: Duration::from_secs(1000),
        ..natpmp_opts(&gw)
    };
    let running = start(&[(Tcp, 6881), (Udp, 6881), (Udp, 7000)], NATPMP, opts);
    wait_mapped(&running, &mapped, method, EXTERNAL_IP.into()).await;

    let started = Instant::now();
    gw.reset_epoch();
    assert_eq!(gw.mappings(), vec![]);
    // The next answer about the refused port shows the epoch going back; everything is mapped
    // again long before a renewal would have done it.
    wait_until(SECOND * 1000 / 2, || {
        let ports = gateway_ports(&gw);
        if ports.len() == 2 {
            Ok(())
        } else {
            Err(format!("{ports:?}"))
        }
    })
    .await;
    assert!(started.elapsed() < SECOND * 1000 / 2);
    running.stop().await;
}

#[tokio::test]
async fn pcp_epoch_reset_recreates_mappings_at_once() {
    epoch_reset_recreates_mappings_at_once(FakeGatewayProtocols::Both, PortMappingMethod::Pcp)
        .await;
}

#[tokio::test]
async fn natpmp_epoch_reset_recreates_mappings_at_once() {
    epoch_reset_recreates_mappings_at_once(
        FakeGatewayProtocols::NatPmpOnly,
        PortMappingMethod::NatPmp,
    )
    .await;
}

#[tokio::test]
async fn unsupported_pcp_version_falls_back_to_natpmp() {
    let gw = FakeGateway::start(
        Ipv4Addr::LOCALHOST.into(),
        gateway_opts(FakeGatewayProtocols::NatPmpOnly),
    )
    .await
    .unwrap();
    let ports = [(Tcp, 6881), (Udp, 6881)];
    let running = start(&ports, NATPMP, natpmp_opts(&gw));
    wait_mapped(
        &running,
        &ports,
        PortMappingMethod::NatPmp,
        EXTERNAL_IP.into(),
    )
    .await;

    let requests = gw.requests();
    // PCP first (RFC 6887 appendix A), then NAT-PMP after the `UNSUPP_VERSION` answer, which
    // starts with the public address for the status.
    assert!(
        matches!(requests[0], FakeGatewayRequest::PcpMap { .. }),
        "{requests:?}"
    );
    assert_eq!(requests[1], FakeGatewayRequest::NatPmpPublicAddress);
    assert!(
        matches!(requests[2], FakeGatewayRequest::NatPmpMap { .. }),
        "{requests:?}"
    );
    assert_eq!(
        gateway_ports(&gw),
        vec![
            (Tcp, 6881, 6881, PortMappingMethod::NatPmp),
            (Udp, 6881, 6881, PortMappingMethod::NatPmp),
        ]
    );
    // One PCP attempt is enough to learn that the gateway does not speak it.
    let pcp = requests
        .iter()
        .filter(|r| matches!(r, FakeGatewayRequest::PcpMap { .. }))
        .count();
    assert_eq!(pcp, 1);

    running.stop().await;
    assert_eq!(gw.mappings(), vec![]);
    let mut deleted = deletions(&gw);
    deleted.sort();
    assert_eq!(deleted, vec![(Tcp, 6881, false), (Udp, 6881, false)]);
}

#[tokio::test]
async fn pcp_only_gateway_maps_with_pcp() {
    let gw = FakeGateway::start(
        Ipv4Addr::LOCALHOST.into(),
        gateway_opts(FakeGatewayProtocols::PcpOnly),
    )
    .await
    .unwrap();
    let ports = [(Tcp, 6881), (Udp, 6881)];
    let running = start(&ports, NATPMP, natpmp_opts(&gw));
    wait_mapped(&running, &ports, PortMappingMethod::Pcp, EXTERNAL_IP.into()).await;
    running.stop().await;
    assert_eq!(gw.mappings(), vec![]);
}

async fn refusals_are_reported(protocols: FakeGatewayProtocols, refusal: FakeRefusal, text: &str) {
    let gw = FakeGateway::start(Ipv4Addr::LOCALHOST.into(), gateway_opts(protocols))
        .await
        .unwrap();
    gw.refuse(Tcp, 6881, refusal);
    let running = start(&[(Tcp, 6881), (Udp, 6881)], NATPMP, natpmp_opts(&gw));
    wait_until(DEADLINE, || {
        let why = failed_reason(&running.row(Tcp, 6881))?;
        if !why.contains(text) {
            return Err(why);
        }
        let udp = running.row(Udp, 6881);
        if udp.state != PortMappingState::Mapped {
            return Err(format!("{udp:?}"));
        }
        Ok(())
    })
    .await;
    running.stop().await;
    // Only the granted port is deleted.
    assert_eq!(
        deletions(&gw)
            .into_iter()
            .map(|(p, port, _)| (p, port))
            .collect::<Vec<_>>(),
        vec![(Udp, 6881)]
    );
}

#[tokio::test]
async fn pcp_refusals_are_reported() {
    refusals_are_reported(
        FakeGatewayProtocols::Both,
        FakeRefusal::NotAuthorized,
        "NOT_AUTHORIZED (2)",
    )
    .await;
}

#[tokio::test]
async fn natpmp_refusals_are_reported() {
    refusals_are_reported(
        FakeGatewayProtocols::NatPmpOnly,
        FakeRefusal::OutOfResources,
        "out of resources",
    )
    .await;
}

#[tokio::test]
async fn different_external_port_is_reported() {
    for (protocols, method) in [
        (FakeGatewayProtocols::Both, PortMappingMethod::Pcp),
        (FakeGatewayProtocols::NatPmpOnly, PortMappingMethod::NatPmp),
    ] {
        let gw = FakeGateway::start(Ipv4Addr::LOCALHOST.into(), gateway_opts(protocols))
            .await
            .unwrap();
        gw.set_external_port(Tcp, 6881, 16881);
        let running = start(&[(Tcp, 6881), (Udp, 6881)], NATPMP, natpmp_opts(&gw));
        wait_until(DEADLINE, || {
            expect_mapped(&running.row(Tcp, 6881), method, 16881, EXTERNAL_IP.into())?;
            expect_mapped(&running.row(Udp, 6881), method, 6881, EXTERNAL_IP.into())
        })
        .await;
        // Renewals ask for the port the gateway picked.
        wait_until(DEADLINE, || {
            let asked = gw.requests().iter().any(|r| match r {
                FakeGatewayRequest::PcpMap {
                    protocol: Tcp,
                    suggested_external_port,
                    ..
                }
                | FakeGatewayRequest::NatPmpMap {
                    protocol: Tcp,
                    suggested_external_port,
                    ..
                } => *suggested_external_port == 16881,
                _ => false,
            });
            if asked {
                Ok(())
            } else {
                Err("no renewal yet".to_owned())
            }
        })
        .await;
        running.stop().await;
    }
}

#[tokio::test]
async fn retransmits_when_datagrams_are_dropped() {
    let gw = FakeGateway::start(
        Ipv4Addr::LOCALHOST.into(),
        gateway_opts(FakeGatewayProtocols::Both),
    )
    .await
    .unwrap();
    gw.drop_next(2);
    let running = start(&[(Tcp, 6881)], NATPMP, natpmp_opts(&gw));
    wait_mapped(
        &running,
        &[(Tcp, 6881)],
        PortMappingMethod::Pcp,
        EXTERNAL_IP.into(),
    )
    .await;
    let requests = gw.requests();
    // The same request (same nonce) three times: two dropped, the third answered.
    assert!(
        matches!(requests[0], FakeGatewayRequest::PcpMap { .. }),
        "{requests:?}"
    );
    assert_eq!(requests[0], requests[1]);
    assert_eq!(requests[0], requests[2]);
    running.stop().await;
}

#[tokio::test]
async fn silent_gateway_is_reported() {
    let gw = FakeGateway::start(
        Ipv4Addr::LOCALHOST.into(),
        gateway_opts(FakeGatewayProtocols::Both),
    )
    .await
    .unwrap();
    gw.set_silent(true);
    let running = start(&[(Tcp, 6881)], NATPMP, natpmp_opts(&gw));
    assert_eq!(running.row(Tcp, 6881).state, PortMappingState::Pending);
    wait_until(DEADLINE, || {
        let why = failed_reason(&running.row(Tcp, 6881))?;
        if why.contains("no answer after 4 attempts") {
            Ok(())
        } else {
            Err(why)
        }
    })
    .await;
    // PCP was tried with its schedule, then NAT-PMP.
    let requests = gw.requests();
    assert!(
        requests
            .iter()
            .any(|r| matches!(r, FakeGatewayRequest::PcpMap { .. }))
    );
    assert!(requests.contains(&FakeGatewayRequest::NatPmpPublicAddress));
    // Nothing was mapped, so nothing is deleted.
    running.stop().await;
    assert_eq!(deletions(&gw), vec![]);
}

#[tokio::test]
async fn shutdown_is_bounded_when_the_gateway_goes_silent() {
    let gw = FakeGateway::start(
        Ipv4Addr::LOCALHOST.into(),
        gateway_opts(FakeGatewayProtocols::Both),
    )
    .await
    .unwrap();
    let opts = PortMappingOptions {
        shutdown_timeout: Duration::from_millis(300),
        ..natpmp_opts(&gw)
    };
    let running = start(&PORTS, NATPMP, opts);
    wait_mapped(&running, &PORTS, PortMappingMethod::Pcp, EXTERNAL_IP.into()).await;
    gw.set_silent(true);
    // Deleting three ports one after the other would take 3 x 300 ms of retransmissions.
    let took = running.stop().await;
    assert!(took < Duration::from_secs(1), "shutdown took {took:?}");
    assert!(!deletions(&gw).is_empty());
}

#[tokio::test]
async fn failed_renewal_discovers_the_gateway_again() {
    let gw = FakeGateway::start(
        Ipv4Addr::LOCALHOST.into(),
        gateway_opts(FakeGatewayProtocols::Both),
    )
    .await
    .unwrap();
    let running = start(&[(Tcp, 6881)], NATPMP, natpmp_opts(&gw));
    wait_mapped(
        &running,
        &[(Tcp, 6881)],
        PortMappingMethod::Pcp,
        EXTERNAL_IP.into(),
    )
    .await;
    // The gateway goes away: the next renewal finds nobody and the port is no longer mapped.
    gw.set_silent(true);
    wait_until(DEADLINE, || match running.row(Tcp, 6881).state {
        PortMappingState::Mapped => Err("still mapped".to_owned()),
        _ => Ok(()),
    })
    .await;
    // It comes back (after a reboot, with an empty table): discovery finds it and maps again.
    gw.reset_epoch();
    gw.set_silent(false);
    wait_mapped(
        &running,
        &[(Tcp, 6881)],
        PortMappingMethod::Pcp,
        EXTERNAL_IP.into(),
    )
    .await;
    assert_eq!(
        gateway_ports(&gw),
        vec![(Tcp, 6881, 6881, PortMappingMethod::Pcp)]
    );
    running.stop().await;
    assert_eq!(gw.mappings(), vec![]);
}

#[tokio::test]
async fn ipv6_pcp_opens_a_pinhole() {
    let gw = match FakeGateway::start(
        Ipv6Addr::LOCALHOST.into(),
        gateway_opts(FakeGatewayProtocols::Both),
    )
    .await
    {
        Ok(gw) => gw,
        Err(e) => {
            // Some CI containers have no IPv6 loopback.
            eprintln!("skipping, no IPv6 loopback: {e}");
            return;
        }
    };
    let ports = [(Tcp, 6881), (Udp, 6881)];
    let running = start(&ports, NATPMP, natpmp_opts(&gw));
    // A pinhole keeps our own address and port.
    wait_mapped(
        &running,
        &ports,
        PortMappingMethod::Pcp,
        Ipv6Addr::LOCALHOST.into(),
    )
    .await;
    running.stop().await;
    assert_eq!(gw.mappings(), vec![]);
}

// -------------------------------------------------------------------------------------------
// UPnP

fn upnp_opts(igd: &FakeIgd) -> PortMappingOptions {
    PortMappingOptions {
        upnp_locations: vec![igd.location()],
        ..opts()
    }
}

fn igd_calls(igd: &FakeIgd, action: &str) -> Vec<crate::test_util::FakeIgdCall> {
    igd.calls()
        .into_iter()
        .filter(|c| c.action == action)
        .collect()
}

#[tokio::test]
async fn upnp_maps_tcp_and_udp_on_every_service_type() {
    for service_type in igd::WAN_SERVICE_TYPES {
        let igd = FakeIgd::start(FakeIgdOptions {
            service_type: service_type.to_owned(),
            ..Default::default()
        })
        .await
        .unwrap();
        let ports = [(Tcp, 6881), (Udp, 6881)];
        let running = start(&ports, UPNP, upnp_opts(&igd));
        wait_mapped(
            &running,
            &ports,
            PortMappingMethod::Upnp,
            EXTERNAL_IP.into(),
        )
        .await;

        let get_ip = igd_calls(&igd, "GetExternalIPAddress");
        assert!(!get_ip.is_empty());
        assert_eq!(
            get_ip[0].soap_action,
            format!("{service_type}#GetExternalIPAddress")
        );
        let adds = igd_calls(&igd, "AddPortMapping");
        for protocol in ["TCP", "UDP"] {
            let add = adds
                .iter()
                .find(|c| c.args["NewProtocol"] == protocol)
                .unwrap_or_else(|| panic!("no {protocol} AddPortMapping in {adds:?}"));
            assert_eq!(add.soap_action, format!("{service_type}#AddPortMapping"));
            let args: Vec<(&str, &str)> = add
                .args
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            assert_eq!(
                args,
                vec![
                    ("NewEnabled", "1"),
                    ("NewExternalPort", "6881"),
                    ("NewInternalClient", "127.0.0.1"),
                    ("NewInternalPort", "6881"),
                    ("NewLeaseDuration", "40"),
                    ("NewPortMappingDescription", "rqbit test"),
                    ("NewProtocol", protocol),
                    ("NewRemoteHost", ""),
                ]
            );
            assert!(
                add.body.contains(&format!("xmlns:u=\"{service_type}\"")),
                "{}",
                add.body
            );
        }
        assert_eq!(igd.mappings().len(), 2);

        running.stop().await;
        assert_eq!(igd.mappings(), vec![]);
        let mut deleted: Vec<_> = igd_calls(&igd, "DeletePortMapping")
            .into_iter()
            .map(|c| {
                (
                    c.args["NewProtocol"].clone(),
                    c.args["NewExternalPort"].clone(),
                )
            })
            .collect();
        deleted.sort();
        assert_eq!(
            deleted,
            vec![
                ("TCP".to_owned(), "6881".to_owned()),
                ("UDP".to_owned(), "6881".to_owned())
            ]
        );
    }
}

#[tokio::test]
async fn upnp_retries_with_a_permanent_lease() {
    let igd = FakeIgd::start(FakeIgdOptions {
        only_permanent_leases: true,
        ..Default::default()
    })
    .await
    .unwrap();
    let ports = [(Tcp, 6881), (Udp, 6881)];
    let running = start(&ports, UPNP, upnp_opts(&igd));
    wait_mapped(
        &running,
        &ports,
        PortMappingMethod::Upnp,
        EXTERNAL_IP.into(),
    )
    .await;
    let leases: Vec<String> = igd_calls(&igd, "AddPortMapping")
        .into_iter()
        .map(|c| c.args["NewLeaseDuration"].clone())
        .collect();
    // Refused once with 725, then the permanent lease for both ports and every refresh.
    assert_eq!(leases[0], "40");
    assert!(leases[1..].iter().all(|l| l == "0"), "{leases:?}");
    assert!(igd.mappings().iter().all(|m| m.lease == 0));
    running.stop().await;
    assert_eq!(igd.mappings(), vec![]);
}

#[tokio::test]
async fn upnp_conflict_is_reported_and_not_taken_over() {
    let igd = FakeIgd::start(FakeIgdOptions::default()).await.unwrap();
    igd.add_foreign_mapping(Tcp, 6881);
    let running = start(&[(Tcp, 6881), (Udp, 6881)], UPNP, upnp_opts(&igd));
    wait_until(DEADLINE, || {
        let why = failed_reason(&running.row(Tcp, 6881))?;
        if !why.contains("718") {
            return Err(why);
        }
        expect_mapped(
            &running.row(Udp, 6881),
            PortMappingMethod::Upnp,
            6881,
            EXTERNAL_IP.into(),
        )
    })
    .await;
    // The conflicting port is never deleted: it is not ours.
    running.stop().await;
    let deleted: Vec<String> = igd_calls(&igd, "DeletePortMapping")
        .into_iter()
        .map(|c| c.args["NewProtocol"].clone())
        .collect();
    assert_eq!(deleted, vec!["UDP".to_owned()]);
}

#[tokio::test]
async fn upnp_is_preferred_when_every_method_mapped() {
    let igd = FakeIgd::start(FakeIgdOptions::default()).await.unwrap();
    let gw = FakeGateway::start(
        Ipv4Addr::LOCALHOST.into(),
        gateway_opts(FakeGatewayProtocols::Both),
    )
    .await
    .unwrap();
    // NAT-PMP / PCP gives TCP another external port; UPnP keeps it.
    gw.set_external_port(Tcp, 6881, 16881);
    let opts = PortMappingOptions {
        upnp_locations: vec![igd.location()],
        ..natpmp_opts(&gw)
    };
    let both = PortMappingMethods {
        upnp: true,
        natpmp: true,
    };
    let running = start(&[(Tcp, 6881), (Udp, 6881)], both, opts);
    wait_until(DEADLINE, || {
        if gateway_ports(&gw).len() != 2 || igd.mappings().len() != 2 {
            return Err("not mapped everywhere yet".to_owned());
        }
        Ok(())
    })
    .await;
    wait_mapped(
        &running,
        &[(Tcp, 6881), (Udp, 6881)],
        PortMappingMethod::Upnp,
        EXTERNAL_IP.into(),
    )
    .await;
    running.stop().await;
    assert_eq!(igd.mappings(), vec![]);
    assert_eq!(gw.mappings(), vec![]);
}

#[tokio::test]
async fn status_after_stop_and_without_methods() {
    let gw = FakeGateway::start(
        Ipv4Addr::LOCALHOST.into(),
        gateway_opts(FakeGatewayProtocols::Both),
    )
    .await
    .unwrap();
    let running = start(&[(Tcp, 6881)], NATPMP, natpmp_opts(&gw));
    wait_mapped(
        &running,
        &[(Tcp, 6881)],
        PortMappingMethod::Pcp,
        EXTERNAL_IP.into(),
    )
    .await;
    let mapper = running.mapper.clone();
    running.stop().await;
    assert_eq!(
        mapper.status(),
        vec![PortMappingStatus {
            protocol: Tcp,
            internal_port: 6881,
            external_port: None,
            method: None,
            external_ip: None,
            state: PortMappingState::Failed("port mapping stopped".to_owned()),
        }]
    );
}
