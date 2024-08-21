use std::net::SocketAddr;

use anyhow::Context;
use tracing::{info, warn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let sock = tokio::net::UdpSocket::bind("0.0.0.0:1900").await
        .context("error converting socket2 socket to tokio")?;

    let sock = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None)
        .context("error creating socket")?;
    sock.set_reuse_address(true)
        .context("error setting reuseaddr")?;

    let addr: SocketAddr = "239.255.255.250:1900".parse().context("invalid addr")?;
    sock.bind(&addr.into()).context("error binding")?;
    sock.set_nonblocking(true)
        .context("error setting non-blocking")?;

    let sock = tokio::net::UdpSocket::from_std(sock.into())
        .context("error converting socket2 socket to tokio")?;

    sock.join_multicast_v4(multiaddr, interface)

    let mut buf = vec![0u8; 16184];
    loop {
        warn!("trying to recv");
        let (sz, addr) = sock.recv_from(&mut buf).await.context("error receiving")?;
        warn!("received!");
        let msg = std::str::from_utf8(&buf[..sz]).context("bad utf-8")?;
        info!(content = msg, ?addr, "received message");
    }
}
