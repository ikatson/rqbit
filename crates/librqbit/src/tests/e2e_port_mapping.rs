//! Router port mapping of a session: the ports it maps (TCP listen port, uTP UDP port, DHT UDP
//! port) against a fake NAT-PMP / PCP gateway and a fake UPnP internet gateway device, both on
//! loopback, and their removal when the session stops.

use std::{
    net::{IpAddr, Ipv4Addr},
    time::Duration,
};

use anyhow::{Context, bail};
use librqbit_upnp::test_util::{FakeGateway, FakeGatewayOptions, FakeIgd, FakeIgdOptions};
use tempfile::TempDir;

use crate::{
    DhtSessionConfig, ListenerMode, ListenerOptions, PortMappingMethod, PortMappingOptions,
    PortMappingProtocol, PortMappingState, PortMappingStatus, Session, SessionOptions,
    tests::test_util::wait_until,
};

/// What both fakes report as the public address.
const EXTERNAL_IP: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 7);

#[tokio::test]
async fn session_maps_tcp_utp_and_dht_ports_and_deletes_them_on_stop() -> anyhow::Result<()> {
    let gateway = FakeGateway::start(Ipv4Addr::LOCALHOST.into(), FakeGatewayOptions::default())
        .await
        .context("starting the fake NAT-PMP / PCP gateway")?;
    let igd = FakeIgd::start(FakeIgdOptions::default())
        .await
        .context("starting the fake IGD")?;
    // The DHT bootstraps from a socket that never answers, so nothing leaves the host.
    let silent_node = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;

    let dir = TempDir::with_prefix("rqbit_port_mapping")?;
    let session = Session::new_with_opts(
        dir.path().into(),
        SessionOptions {
            dht: Some(DhtSessionConfig {
                bootstrap_addrs: Some(vec![silent_node.local_addr()?.to_string()]),
                port: None,
                persistence: None,
            }),
            persistence: None,
            listen: Some(ListenerOptions {
                mode: ListenerMode::TcpAndUtp,
                listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
                ipv4_only: true,
                enable_upnp_port_forwarding: true,
                enable_natpmp_port_forwarding: true,
                port_mapping: Some(PortMappingOptions {
                    gateways: vec![gateway.addr().ip()],
                    gateway_port: gateway.addr().port(),
                    upnp_locations: vec![igd.location()],
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ipv4_only: true,
            disable_local_service_discovery: true,
            ..Default::default()
        },
    )
    .await
    .context("error creating the session")?;

    let peer_port = session.listen_addr().context("no listen address")?.port();
    let dht_port = session.get_dht().context("no DHT")?.listen_addr().port();
    assert_ne!(peer_port, dht_port);

    let mut expected: Vec<PortMappingStatus> = [
        (PortMappingProtocol::Tcp, peer_port),
        (PortMappingProtocol::Udp, peer_port),
        (PortMappingProtocol::Udp, dht_port),
    ]
    .into_iter()
    .map(|(protocol, port)| PortMappingStatus {
        protocol,
        internal_port: port,
        external_port: Some(port),
        // Every method mapped every port; the row reports the preferred one.
        method: Some(PortMappingMethod::Upnp),
        external_ip: Some(IpAddr::V4(EXTERNAL_IP)),
        state: PortMappingState::Mapped,
    })
    .collect();
    expected.sort_by_key(|s| (s.protocol, s.internal_port));

    let mut expected_ports: Vec<(PortMappingProtocol, u16)> = expected
        .iter()
        .map(|s| (s.protocol, s.internal_port))
        .collect();
    expected_ports.sort();
    wait_until(
        || {
            let mut on_gateway: Vec<_> = gateway
                .mappings()
                .into_iter()
                .map(|m| (m.protocol, m.internal_port))
                .collect();
            on_gateway.sort();
            let mut on_igd: Vec<_> = igd
                .mappings()
                .into_iter()
                .map(|m| {
                    let protocol = if m.protocol == "TCP" {
                        PortMappingProtocol::Tcp
                    } else {
                        PortMappingProtocol::Udp
                    };
                    (protocol, m.internal_port)
                })
                .collect();
            on_igd.sort();
            if on_gateway != expected_ports || on_igd != expected_ports {
                bail!("gateway has {on_gateway:?}, IGD has {on_igd:?}");
            }
            let status = session.port_mappings();
            if status != expected {
                bail!("status is {status:?}");
            }
            Ok(())
        },
        Duration::from_secs(10),
    )
    .await?;
    // Mapping does not change what the session announces.
    assert_eq!(session.announce_port(), None);

    session.stop().await;
    assert_eq!(gateway.mappings(), vec![]);
    assert_eq!(igd.mappings(), vec![]);
    assert!(
        session
            .port_mappings()
            .iter()
            .all(|s| s.state != PortMappingState::Mapped),
        "{:?}",
        session.port_mappings()
    );
    Ok(())
}
