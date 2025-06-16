use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};

use librqbit_dualstack_sockets::UdpSocket;
use tokio_util::sync::CancellationToken;

pub struct LocalServiceDiscovery {
    socket: UdpSocket,
    cookie: u32
}

#[derive(Default)]
pub struct LocalServiceDiscoveryOptions {
    token: CancellationToken,
    cookie: Option<u32>
}

impl LocalServiceDiscovery {
    pub fn new(announce_port: u16, opts: LocalServiceDiscoveryOptions) -> anyhow::Result<Self> {
        let socket = UdpSocket::bind_udp(
            SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0)),
            true,
        )?;

        socket.socket().join_multicast_v4(multiaddr, interface)

        Ok(Self { socket })
    }

    async fn run_forever(&self) -> anyhow::Result<()> {

    }
}
