//! Finding NAT-PMP / PCP gateways without a routing-table dependency.
//!
//! Both RFCs talk to "the default gateway", but no portable API in std (or in our dependencies)
//! reports it. So we build a list of addresses that are likely to be the gateway and ask each of
//! them for the public address (RFC 6886 section 3.2); whoever answers is a gateway.

use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};

use futures::{StreamExt, stream::FuturesUnordered};
use librqbit_dualstack_sockets::BindDevice;
use network_interface::{Addr, NetworkInterface};
use tracing::{debug, trace};

use crate::{
    natpmp, pcp,
    transport::{GatewaySocket, Retransmit},
};

/// The Linux routing table as text, with the default routes in it.
#[cfg(any(target_os = "linux", target_os = "android"))]
const PROC_NET_ROUTE: &str = "/proc/net/route";

/// Parses `/proc/net/route` and returns the gateways of the IPv4 default routes, in the order of
/// the table. Lines that do not parse are skipped: the file is read as a hint, never trusted.
///
/// The kernel prints addresses as the hex of the `u32` it holds in network byte order, read as a
/// native integer, so the bytes come back with `to_ne_bytes`.
// Only Linux and Android have the file; the parser is tested everywhere.
#[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
pub(crate) fn parse_proc_net_route(text: &str) -> Vec<Ipv4Addr> {
    // `RTF_UP | RTF_GATEWAY` from `<linux/route.h>`: a usable route through a gateway.
    const RTF_UP: u32 = 0x1;
    const RTF_GATEWAY: u32 = 0x2;
    let hex = |s: &str| u32::from_str_radix(s, 16).ok();
    let mut gateways = Vec::new();
    // The first line is the column header.
    for line in text.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let (Some(_iface), Some(dest), Some(gateway), Some(flags)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        // RefCnt, Use, Metric come before the mask.
        let Some(mask) = fields.nth(3) else {
            continue;
        };
        let (Some(dest), Some(gateway), Some(flags), Some(mask)) =
            (hex(dest), hex(gateway), hex(flags), hex(mask))
        else {
            continue;
        };
        if dest != 0 || mask != 0 || flags & (RTF_UP | RTF_GATEWAY) != RTF_UP | RTF_GATEWAY {
            continue;
        }
        let gateway = Ipv4Addr::from(gateway.to_ne_bytes());
        if !gateway.is_unspecified() && !gateways.contains(&gateway) {
            gateways.push(gateway);
        }
    }
    gateways
}

/// Reads the default routes of the host where the kernel exposes them (Linux and Android, when
/// readable: Android may forbid it). Elsewhere there is nothing to read.
pub(crate) fn default_routes() -> Vec<Ipv4Addr> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        match std::fs::read_to_string(PROC_NET_ROUTE) {
            Ok(text) => parse_proc_net_route(&text),
            Err(e) => {
                trace!("cannot read {PROC_NET_ROUTE}: {e:#}");
                Vec::new()
            }
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        Vec::new()
    }
}

/// The first and the last host address of the subnet of `ip`, which is where home routers sit
/// (192.168.1.1, 10.0.0.1, 192.168.1.254...). Subnets of /31 and /32 have no such pair.
fn subnet_edges(ip: Ipv4Addr, netmask: Ipv4Addr) -> Option<[Ipv4Addr; 2]> {
    let mask = netmask.to_bits();
    if mask.leading_ones() > 30 || mask.count_ones() != mask.leading_ones() {
        return None;
    }
    let network = ip.to_bits() & mask;
    let broadcast = network | !mask;
    Some([
        Ipv4Addr::from_bits(network + 1),
        Ipv4Addr::from_bits(broadcast - 1),
    ])
}

/// Builds the list of addresses that may be a NAT-PMP / PCP gateway, without duplicates, in this
/// order:
///
/// 1. the host of the UPnP internet gateway device found by SSDP (`igd_hosts`): a router that
///    speaks UPnP very often speaks NAT-PMP / PCP too;
/// 2. the gateways of the IPv4 default routes (`default_routes`, from `/proc/net/route` where
///    readable);
/// 3. for every non-loopback interface with a private IPv4 address (RFC 1918), the first and the
///    last host address of its subnet. `network-interface` does not report whether an interface
///    is up; having an address stands in for it.
///
/// With a bound device only the interface of that index is looked at. Nothing is generated for
/// IPv6: a PCP server there has to be given in `PortMappingOptions::gateways`.
pub(crate) fn gateway_candidates(
    interfaces: &[NetworkInterface],
    default_routes: &[Ipv4Addr],
    igd_hosts: &[Ipv4Addr],
    bind_device_index: Option<u32>,
) -> Vec<Ipv4Addr> {
    let own: HashSet<Ipv4Addr> = interfaces
        .iter()
        .flat_map(|i| i.addr.iter())
        .filter_map(|a| match a {
            Addr::V4(a) => Some(a.ip),
            Addr::V6(_) => None,
        })
        .collect();
    let from_subnets = interfaces
        .iter()
        .filter(|i| !i.internal)
        .filter(|i| bind_device_index.is_none_or(|idx| idx == i.index))
        .flat_map(|i| i.addr.iter())
        .filter_map(|a| match a {
            Addr::V4(a) if a.ip.is_private() => subnet_edges(a.ip, a.netmask?),
            _ => None,
        })
        .flatten();

    let mut seen = HashSet::new();
    igd_hosts
        .iter()
        .chain(default_routes)
        .copied()
        .chain(from_subnets)
        .filter(|ip| !ip.is_unspecified() && !ip.is_loopback() && !own.contains(ip))
        .filter(|ip| seen.insert(*ip))
        .collect()
}

/// A candidate that answered the probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FoundGateway {
    pub addr: IpAddr,
    /// The public address, when the gateway answered the NAT-PMP request with one.
    pub external_ip: Option<Ipv4Addr>,
}

/// Recognises an answer to the public address request. NAT-PMP gateways answer it (with an
/// error code if they have no public address yet); a PCP-only server answers it with a PCP
/// `UNSUPP_VERSION` (RFC 6887 section 9). Either way something speaks the protocol there.
fn probe_answer(buf: &[u8]) -> Option<Option<Ipv4Addr>> {
    if let Ok(r) = natpmp::Response::decode(buf) {
        return match r.body {
            natpmp::ResponseBody::PublicAddress(ip) if r.result == natpmp::ResultCode::Success => {
                Some(Some(ip))
            }
            _ if r.opcode == natpmp::OP_PUBLIC_ADDRESS => Some(None),
            _ => None,
        };
    }
    pcp::is_unsupported_version_answer(buf).then_some(None)
}

async fn probe_one(
    addr: Ipv4Addr,
    port: u16,
    retransmit: Retransmit,
    bind_device: Option<&BindDevice>,
) -> Option<FoundGateway> {
    let target = SocketAddr::new(IpAddr::V4(addr), port);
    let socket = match GatewaySocket::connect(target, bind_device).await {
        Ok(s) => s,
        Err(e) => {
            debug!(%target, "cannot probe: {e:#}");
            return None;
        }
    };
    let request = natpmp::Request::PublicAddress.encode();
    match socket.exchange(&request, retransmit, probe_answer).await {
        Ok(external_ip) => Some(FoundGateway {
            addr: IpAddr::V4(addr),
            external_ip,
        }),
        Err(e) => {
            trace!(%target, "no NAT-PMP / PCP gateway: {e}");
            None
        }
    }
}

/// Probes all candidates at once and returns those that answered, in candidate order. The whole
/// probe ends after `timeout`, so guesses that never answer do not hold discovery for the full
/// retransmission schedule.
pub(crate) async fn probe(
    candidates: &[Ipv4Addr],
    port: u16,
    retransmit: Retransmit,
    timeout: Duration,
    bind_device: Option<&BindDevice>,
) -> Vec<FoundGateway> {
    let mut probes: FuturesUnordered<_> = candidates
        .iter()
        .map(|c| probe_one(*c, port, retransmit, bind_device))
        .collect();
    let mut found = Vec::new();
    let collect = async {
        while let Some(r) = probes.next().await {
            found.extend(r);
        }
    };
    // Running out of time just ends the probe with what answered so far.
    if tokio::time::timeout(timeout, collect).await.is_err() {
        trace!("gateway probe timed out, {} answered", found.len());
    }
    found.sort_by_key(|f| {
        candidates
            .iter()
            .position(|c| IpAddr::V4(*c) == f.addr)
            .unwrap_or(usize::MAX)
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route_hex(ip: Ipv4Addr) -> String {
        format!("{:08X}", u32::from_ne_bytes(ip.octets()))
    }

    #[test]
    fn test_parse_proc_net_route() {
        let gw = route_hex(Ipv4Addr::new(192, 168, 1, 1));
        let gw2 = route_hex(Ipv4Addr::new(10, 0, 0, 1));
        let net = route_hex(Ipv4Addr::new(192, 168, 1, 0));
        let mask24 = route_hex(Ipv4Addr::new(255, 255, 255, 0));
        let text = format!(
            "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
             eth0\t00000000\t{gw}\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
             eth0\t{net}\t00000000\t0001\t0\t0\t100\t{mask24}\t0\t0\t0\n\
             wlan0\t00000000\t{gw2}\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
             \n\
             garbage\n\
             eth1\t00000000\tZZZZZZZZ\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
             eth1\t00000000\t{gw}\n\
             eth2\t00000000\t{gw2}\t0001\t0\t0\t100\t00000000\t0\t0\t0\n\
             eth3\t00000000\t{gw}\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
             eth4\t00000000\t00000000\t0003\t0\t0\t100\t00000000\t0\t0\t0\n"
        );
        assert_eq!(
            parse_proc_net_route(&text),
            vec![Ipv4Addr::new(192, 168, 1, 1), Ipv4Addr::new(10, 0, 0, 1)]
        );
        assert!(parse_proc_net_route("").is_empty());
        assert!(parse_proc_net_route("only a header").is_empty());
    }

    #[test]
    fn test_subnet_edges() {
        let ip = Ipv4Addr::new(192, 168, 1, 37);
        assert_eq!(
            subnet_edges(ip, Ipv4Addr::new(255, 255, 255, 0)),
            Some([
                Ipv4Addr::new(192, 168, 1, 1),
                Ipv4Addr::new(192, 168, 1, 254)
            ])
        );
        assert_eq!(
            subnet_edges(Ipv4Addr::new(10, 1, 2, 3), Ipv4Addr::new(255, 0, 0, 0)),
            Some([Ipv4Addr::new(10, 0, 0, 1), Ipv4Addr::new(10, 255, 255, 254)])
        );
        assert_eq!(
            subnet_edges(ip, Ipv4Addr::new(255, 255, 255, 252)),
            Some([
                Ipv4Addr::new(192, 168, 1, 37),
                Ipv4Addr::new(192, 168, 1, 38)
            ])
        );
        assert_eq!(subnet_edges(ip, Ipv4Addr::new(255, 255, 255, 254)), None);
        assert_eq!(subnet_edges(ip, Ipv4Addr::new(255, 255, 255, 255)), None);
        // Not a contiguous mask.
        assert_eq!(subnet_edges(ip, Ipv4Addr::new(255, 0, 255, 0)), None);
    }

    #[test]
    fn test_gateway_candidates() {
        let mask24 = Some(Ipv4Addr::new(255, 255, 255, 0));
        let mut wifi = NetworkInterface::new_afinet(
            "en0",
            Ipv4Addr::new(192, 168, 1, 37),
            mask24,
            None,
            4,
            false,
        );
        wifi.addr.push(Addr::V6(network_interface::V6IfAddr {
            ip: "fe80::1".parse().unwrap(),
            broadcast: None,
            netmask: None,
        }));
        let interfaces = vec![
            NetworkInterface::new_afinet(
                "lo0",
                Ipv4Addr::LOCALHOST,
                Some(Ipv4Addr::new(255, 0, 0, 0)),
                None,
                1,
                true,
            ),
            wifi,
            // Not private: a host with a public address is not behind a NAT on that interface.
            NetworkInterface::new_afinet("en1", Ipv4Addr::new(8, 8, 4, 4), mask24, None, 5, false),
            NetworkInterface::new_afinet(
                "en2",
                Ipv4Addr::new(10, 0, 0, 2),
                Some(Ipv4Addr::new(255, 255, 255, 0)),
                None,
                6,
                false,
            ),
            // No netmask: no subnet to guess from.
            NetworkInterface::new_afinet("en3", Ipv4Addr::new(172, 16, 0, 9), None, None, 7, false),
            // The host owns 192.168.5.1, so that edge is not a gateway.
            NetworkInterface::new_afinet(
                "br0",
                Ipv4Addr::new(192, 168, 5, 1),
                mask24,
                None,
                8,
                false,
            ),
        ];
        let routes = [Ipv4Addr::new(10, 0, 0, 1), Ipv4Addr::new(192, 168, 1, 1)];
        let igd = [Ipv4Addr::new(192, 168, 1, 1)];

        assert_eq!(
            gateway_candidates(&interfaces, &routes, &igd, None),
            vec![
                Ipv4Addr::new(192, 168, 1, 1),
                Ipv4Addr::new(10, 0, 0, 1),
                Ipv4Addr::new(192, 168, 1, 254),
                Ipv4Addr::new(10, 0, 0, 254),
                Ipv4Addr::new(192, 168, 5, 254),
            ]
        );
        assert_eq!(
            gateway_candidates(&interfaces, &[], &[], Some(6)),
            vec![Ipv4Addr::new(10, 0, 0, 1), Ipv4Addr::new(10, 0, 0, 254)]
        );
        assert!(gateway_candidates(&[], &[], &[], None).is_empty());
    }

    async fn probe_fake(
        gateway: &crate::test_util::FakeGateway,
        timeout: Duration,
    ) -> Vec<FoundGateway> {
        let retransmit = Retransmit {
            initial: Duration::from_millis(20),
            attempts: 3,
        };
        probe(
            &[Ipv4Addr::LOCALHOST],
            gateway.addr().port(),
            retransmit,
            timeout,
            None,
        )
        .await
    }

    #[tokio::test]
    async fn test_probe() {
        use crate::test_util::{FakeGateway, FakeGatewayOptions, FakeGatewayProtocols};
        let external_ip = Ipv4Addr::new(203, 0, 113, 7);
        let start = |protocols| {
            FakeGateway::start(
                Ipv4Addr::LOCALHOST.into(),
                FakeGatewayOptions {
                    protocols,
                    external_ip,
                    ..Default::default()
                },
            )
        };
        let timeout = Duration::from_secs(5);

        // A NAT-PMP gateway tells its public address.
        let gw = start(FakeGatewayProtocols::Both).await.unwrap();
        assert_eq!(
            probe_fake(&gw, timeout).await,
            vec![FoundGateway {
                addr: Ipv4Addr::LOCALHOST.into(),
                external_ip: Some(external_ip),
            }]
        );

        // A PCP-only server answers UNSUPP_VERSION: still a gateway.
        let gw = start(FakeGatewayProtocols::PcpOnly).await.unwrap();
        assert_eq!(
            probe_fake(&gw, timeout).await,
            vec![FoundGateway {
                addr: Ipv4Addr::LOCALHOST.into(),
                external_ip: None,
            }]
        );

        // Silence is no gateway, and the probe ends at its timeout at the latest.
        let gw = start(FakeGatewayProtocols::Both).await.unwrap();
        gw.set_silent(true);
        let started = tokio::time::Instant::now();
        assert_eq!(probe_fake(&gw, Duration::from_millis(50)).await, vec![]);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
