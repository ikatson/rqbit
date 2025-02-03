use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
};

use anyhow::Context;
use librqbit_utp::UtpSocketUdp;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;
use tracing::info;

pub(crate) struct ListenResult {
    pub tcp_socket: Option<TcpListener>,
    pub utp_socket: Option<Arc<UtpSocketUdp>>,
    pub enable_upnp_port_forwarding: bool,
    pub addr: SocketAddr,
    pub announce_port: Option<u16>,
}

#[derive(Debug, Clone, Copy)]
pub enum ListenerMode {
    TcpOnly,
    UtpOnly,
    TcpAndUtp,
}

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
}

impl Default for ListenerOptions {
    fn default() -> Self {
        Self {
            // TODO: once uTP is stable upgrade default to both
            mode: ListenerMode::TcpOnly,
            listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
            enable_upnp_port_forwarding: false,
            utp_opts: None,
        }
    }
}

impl ListenerOptions {
    pub(crate) async fn start(
        mut self,
        parent_span: Option<tracing::Id>,
        cancellation_token: CancellationToken,
    ) -> anyhow::Result<ListenResult> {
        if self.listen_addr.port() == 0 {
            anyhow::bail!("you must set the listen port explicitly")
        }
        let mut utp_opts = self.utp_opts.take().unwrap_or_default();
        utp_opts.cancellation_token = cancellation_token;
        utp_opts.parent_span = parent_span;

        let tcp = async {
            if !self.mode.tcp_enabled() {
                return Ok::<_, anyhow::Error>(None);
            }
            let listener = TcpListener::bind(self.listen_addr)
                .await
                .context("error starting TCP listener")?;
            info!(
                "Listening on TCP {:?} for incoming peer connections",
                self.listen_addr
            );
            Ok(Some(listener))
        };

        let utp = async {
            if !self.mode.utp_enabled() {
                return Ok::<_, anyhow::Error>(None);
            }
            Ok(Some(
                UtpSocketUdp::new_udp_with_opts(self.listen_addr, utp_opts)
                    .await
                    .context("error starting uTP listener")?,
            ))
        };

        let announce_port = if self.listen_addr.ip().is_loopback() {
            None
        } else {
            Some(self.listen_addr.port())
        };
        let (tcp_socket, utp_socket) = tokio::try_join!(tcp, utp)?;
        Ok(ListenResult {
            tcp_socket,
            utp_socket,
            announce_port,
            addr: self.listen_addr,
            enable_upnp_port_forwarding: self.enable_upnp_port_forwarding,
        })
    }
}

pub(crate) trait Accept {
    async fn accept(
        &self,
    ) -> anyhow::Result<(
        SocketAddr,
        (
            impl AsyncRead + Unpin + Send + Sync + 'static,
            (impl AsyncWrite + Unpin + Send + Sync + 'static),
        ),
    )>;
}

impl Accept for TcpListener {
    async fn accept(
        &self,
    ) -> anyhow::Result<(
        SocketAddr,
        (
            impl AsyncRead + Send + Sync + 'static,
            (impl AsyncWrite + Send + Sync + 'static),
        ),
    )> {
        let (stream, addr) = self.accept().await.context("error accepting TCP")?;
        let (read, write) = stream.into_split();
        Ok((addr, (read, write)))
    }
}

impl Accept for Arc<UtpSocketUdp> {
    async fn accept(
        &self,
    ) -> anyhow::Result<(
        SocketAddr,
        (
            impl AsyncRead + Unpin + Send + Sync + 'static,
            impl AsyncWrite + Unpin + Send + Sync + 'static,
        ),
    )> {
        let stream = self.accept().await.context("error accepting uTP")?;
        let addr = stream.remote_addr();
        let (read, write) = stream.split();
        Ok((addr, (read, write)))
    }
}
