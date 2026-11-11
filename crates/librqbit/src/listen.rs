use std::{
    io::ErrorKind,
    net::{Ipv6Addr, SocketAddr},
    sync::Arc,
};

use anyhow::Context;
use librqbit_dualstack_sockets::{BindOpts, TcpListener};
use librqbit_utp::{BindDevice, UtpSocketUdp, UtpSocketUdpOpts};
use tokio::io::AsyncWrite;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::{stream_connect::ConnectionKind, vectored_traits::AsyncReadVectored};

pub(crate) struct ListenResult {
    pub tcp_socket: Option<TcpListener>,
    pub utp_socket: Option<Arc<UtpSocketUdp>>,
    pub enable_upnp_port_forwarding: bool,
    pub addr: SocketAddr,
    pub announce_port: Option<u16>,
    pub max_pending_incoming_handshake_checks: usize,
}

#[derive(Debug, Clone, Copy)]
pub enum ListenerMode {
    TcpOnly,
    UtpOnly,
    TcpAndUtp,
}

pub const DEFAULT_MAX_PENDING_INCOMING_HANDSHAKE_CHECKS: usize = 256;

impl ListenerMode {
    pub fn tcp_enabled(&self) -> bool {
        match self {
            ListenerMode::TcpOnly => true,
            ListenerMode::UtpOnly => false,
            ListenerMode::TcpAndUtp => true,
        }
    }

    pub fn utp_enabled(&self) -> bool {
        match self {
            ListenerMode::TcpOnly => false,
            ListenerMode::UtpOnly => true,
            ListenerMode::TcpAndUtp => true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ListenerOptions {
    pub mode: ListenerMode,
    pub listen_addr: SocketAddr,
    pub enable_upnp_port_forwarding: bool,
    pub utp_opts: Option<librqbit_utp::SocketOpts>,
    pub announce_port: Option<u16>,
    pub ipv4_only: bool,
    pub max_pending_incoming_handshake_checks: usize,
}

impl Default for ListenerOptions {
    fn default() -> Self {
        Self {
            // TODO: once uTP is stable upgrade default to both
            mode: ListenerMode::TcpOnly,
            listen_addr: (Ipv6Addr::UNSPECIFIED, 0).into(),
            enable_upnp_port_forwarding: false,
            utp_opts: None,
            announce_port: None,
            ipv4_only: false,
            max_pending_incoming_handshake_checks: DEFAULT_MAX_PENDING_INCOMING_HANDSHAKE_CHECKS,
        }
    }
}

impl ListenerOptions {
    pub(crate) async fn start(
        mut self,
        parent_span: Option<tracing::Id>,
        cancellation_token: CancellationToken,
        bind_device: Option<&BindDevice>,
    ) -> anyhow::Result<ListenResult> {
        let mut utp_opts = self.utp_opts.take().unwrap_or_default();
        utp_opts.cancellation_token = cancellation_token.clone();
        utp_opts.parent_span = parent_span;
        utp_opts.dont_wait_for_lastack = true;

        let requested_addr = if self.ipv4_only {
            if self.listen_addr.is_ipv6() && self.listen_addr.ip().is_unspecified() {
                // Force to IPv4 unspecified if IPv6 unspecified was requested but we are v4 only
                SocketAddr::from(([0, 0, 0, 0], self.listen_addr.port()))
            } else {
                self.listen_addr
            }
        } else {
            self.listen_addr
        };

        // With any port asked for (port 0), uTP listens on the port the system picked for TCP,
        // which may already be taken over UDP. Then both pick again, rather than lose uTP to bad
        // luck.
        let may_pick_again =
            requested_addr.port() == 0 && self.mode.tcp_enabled() && self.mode.utp_enabled();
        let mut picks = 1;
        let (tcp_socket, utp_socket, listen_addr) = loop {
            let mut listen_addr = requested_addr;
            let tcp_socket = if self.mode.tcp_enabled() {
                let listener = TcpListener::bind_tcp(
                    listen_addr,
                    BindOpts {
                        request_dualstack: !self.ipv4_only,
                        reuseport: false,
                        device: bind_device,
                    },
                )
                .context("error starting TCP listener")?;
                listen_addr = listener.bind_addr();
                Some(listener)
            } else {
                None
            };

            let utp_socket = if self.mode.utp_enabled() {
                let bind_result = UtpSocketUdp::new_udp_with_opts(
                    listen_addr,
                    utp_opts.clone(),
                    UtpSocketUdpOpts { bind_device },
                )
                .await;
                match bind_result {
                    Ok(sock) => {
                        listen_addr = sock.bind_addr();
                        Some(sock)
                    }
                    Err(e) if may_pick_again && picks < MAX_PORT_PICKS && is_addr_in_use(&e) => {
                        debug!("UDP {listen_addr:?} is taken, picking another port");
                        picks += 1;
                        continue;
                    }
                    Err(e) if tcp_socket.is_some() => {
                        // If we listen over TCP, it's not a fatal error if we can't listen over uTP.
                        tracing::error!("Error listening on UDP {listen_addr:?}: {e:#}");
                        None
                    }
                    Err(e) => {
                        return Err(e.into());
                    }
                }
            } else {
                None
            };
            break (tcp_socket, utp_socket, listen_addr);
        };
        if tcp_socket.is_some() {
            info!(
                "Listening on TCP {:?} for incoming peer connections",
                listen_addr
            );
        }
        if utp_socket.is_some() {
            info!(
                "Listening on UDP {:?} for incoming uTP peer connections",
                listen_addr
            );
        }

        let announce_port = if let Some(p) = self.announce_port {
            Some(p)
        } else if listen_addr.ip().is_loopback() {
            None
        } else {
            Some(listen_addr.port())
        };
        Ok(ListenResult {
            tcp_socket,
            utp_socket,
            announce_port,
            addr: listen_addr,
            enable_upnp_port_forwarding: self.enable_upnp_port_forwarding,
            max_pending_incoming_handshake_checks: self.max_pending_incoming_handshake_checks,
        })
    }
}

/// How many ports a listener on any port tries for both TCP and uTP before it gives up uTP.
const MAX_PORT_PICKS: u32 = 16;

/// Whether binding a uTP socket failed because its UDP port is taken.
fn is_addr_in_use(e: &librqbit_utp::Error) -> bool {
    match e {
        librqbit_utp::Error::Dualstack(e) => matches!(
            &**e,
            librqbit_dualstack_sockets::Error::Bind(e) if e.kind() == ErrorKind::AddrInUse
        ),
        _ => false,
    }
}

pub(crate) trait Accept {
    const KIND: ConnectionKind;

    async fn accept(
        &self,
    ) -> anyhow::Result<(
        SocketAddr,
        (
            impl AsyncReadVectored + Send + 'static,
            impl AsyncWrite + Unpin + Send + 'static,
        ),
    )>;
}

impl Accept for TcpListener {
    const KIND: ConnectionKind = ConnectionKind::Tcp;
    async fn accept(
        &self,
    ) -> anyhow::Result<(
        SocketAddr,
        (
            impl AsyncReadVectored + Send + 'static,
            impl AsyncWrite + Send + 'static,
        ),
    )> {
        let (stream, addr) = self.accept().await.context("error accepting TCP")?;
        let (read, write) = stream.into_split();
        Ok((addr, (read, write)))
    }
}

impl Accept for Arc<UtpSocketUdp> {
    const KIND: ConnectionKind = ConnectionKind::Utp;
    async fn accept(
        &self,
    ) -> anyhow::Result<(
        SocketAddr,
        (
            impl AsyncReadVectored + Send + 'static,
            impl AsyncWrite + Unpin + Send + 'static,
        ),
    )> {
        let stream = self.accept().await.context("error accepting uTP")?;
        let addr = stream.remote_addr();
        let (read, write) = stream.split();
        Ok((addr, (read, write)))
    }
}
