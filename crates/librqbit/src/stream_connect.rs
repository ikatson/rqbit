use std::{
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, bail};
use librqbit_utp::UtpSocketUdp;
use socket2::SockRef;
use tracing::debug;

use crate::{
    PeerConnectionOptions,
    type_aliases::{BoxAsyncRead, BoxAsyncWrite},
};

pub enum ConnectionKind {
    Tcp,
    Utp,
    Socks,
}

impl std::fmt::Display for ConnectionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionKind::Tcp => f.write_str("tcp"),
            ConnectionKind::Utp => f.write_str("uTP"),
            ConnectionKind::Socks => f.write_str("socks"),
        }
    }
}

pub struct ConnectionOptions {
    // socks5://[username:password@]host:port
    // If set, all outgoing connections will go through the proxy over TCP.
    pub proxy_url: Option<String>,
    // TCP outgoing connections are enabled by default
    pub enable_tcp: bool,
    pub peer_opts: Option<PeerConnectionOptions>,
}

impl Default for ConnectionOptions {
    fn default() -> Self {
        Self {
            enable_tcp: true,
            proxy_url: None,
            peer_opts: None,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SocksProxyConfig {
    pub host: String,
    pub port: u16,
    pub username_password: Option<(String, String)>,
}

#[derive(Default, Debug, Clone)]
pub(crate) struct StreamConnectorArgs {
    pub enable_tcp: bool,
    pub tcp_source_port: Option<u16>,
    pub socks_proxy_config: Option<SocksProxyConfig>,
    pub utp_socket: Option<Arc<UtpSocketUdp>>,
}

impl SocksProxyConfig {
    pub fn parse(url: &str) -> anyhow::Result<Self> {
        let url = ::url::Url::parse(url).context("invalid proxy URL")?;
        if url.scheme() != "socks5" {
            anyhow::bail!("proxy URL should have socks5 scheme");
        }
        let host = url.host_str().context("missing host")?;
        let port = url.port().context("missing port")?;
        let up = url
            .password()
            .map(|p| (url.username().to_owned(), p.to_owned()));
        Ok(Self {
            host: host.to_owned(),
            port,
            username_password: up,
        })
    }

    async fn connect(
        &self,
        addr: SocketAddr,
    ) -> anyhow::Result<(
        impl tokio::io::AsyncRead + Unpin + 'static,
        impl tokio::io::AsyncWrite + Unpin + 'static,
    )> {
        let proxy_addr = (self.host.as_str(), self.port);

        let stream = if let Some((username, password)) = self.username_password.as_ref() {
            tokio_socks::tcp::Socks5Stream::connect_with_password(
                proxy_addr,
                addr,
                username.as_str(),
                password.as_str(),
            )
            .await
            .context("error connecting to proxy")?
        } else {
            tokio_socks::tcp::Socks5Stream::connect(proxy_addr, addr)
                .await
                .context("error connecting to proxy")?
        };

        Ok(tokio::io::split(stream))
    }
}

#[derive(Debug)]
pub(crate) struct StreamConnector {
    proxy_config: Option<SocksProxyConfig>,
    enable_tcp: bool,
    tcp_source_port: Option<u16>,
    utp_socket: Option<Arc<librqbit_utp::UtpSocketUdp>>,
}

impl StreamConnector {
    pub async fn new(config: StreamConnectorArgs) -> anyhow::Result<Self> {
        #[allow(clippy::single_match)]
        match (
            config.socks_proxy_config.is_some(),
            config.enable_tcp,
            config.utp_socket.is_some(),
        ) {
            (false, false, false) => {
                bail!("no way to connect to peers, enable TCP, uTP or socks proxy")
            }
            _ => {
                // TODO: maybe validate other combinations. For now there's no way to disable TCP
            }
        }

        Ok(Self {
            proxy_config: config.socks_proxy_config,
            enable_tcp: config.enable_tcp,
            tcp_source_port: config.tcp_source_port,
            utp_socket: config.utp_socket,
        })
    }

    async fn tcp_connect(&self, addr: SocketAddr) -> std::io::Result<tokio::net::TcpStream> {
        let (sock, bind_addr) = if addr.is_ipv6() {
            (
                tokio::net::TcpSocket::new_v6()?,
                SocketAddr::from((Ipv6Addr::UNSPECIFIED, self.tcp_source_port.unwrap_or(0))),
            )
        } else {
            (
                tokio::net::TcpSocket::new_v4()?,
                SocketAddr::from((Ipv4Addr::UNSPECIFIED, self.tcp_source_port.unwrap_or(0))),
            )
        };
        let sref = SockRef::from(&sock);

        if bind_addr.port() > 0 {
            #[cfg(not(windows))]
            sref.set_reuse_port(true)?;
            sref.set_reuse_address(true)?;
            sref.bind(&bind_addr.into())?;
        }

        sock.connect(addr).await
    }

    pub async fn connect(
        &self,
        addr: SocketAddr,
    ) -> anyhow::Result<(ConnectionKind, BoxAsyncRead, BoxAsyncWrite)> {
        if let Some(proxy) = self.proxy_config.as_ref() {
            let (r, w) = proxy.connect(addr).await?;
            debug!(?addr, "connected through SOCKS5");
            return Ok((ConnectionKind::Socks, Box::new(r), Box::new(w)));
        }

        // Try to connect over TCP first. If in 1 second we haven't connected, try uTP also (if configured).
        // Whoever connects first wins.

        let tcp_connect = async {
            if !self.enable_tcp {
                bail!("TCP outgoing connections disabled");
            }
            let conn = self.tcp_connect(addr).await?;
            debug!(?addr, "connected over TCP");
            Ok(conn)
        };

        let tcp_failed_notify = tokio::sync::Notify::new();

        let utp_connect = async {
            let sock = match self.utp_socket.as_ref() {
                Some(sock) => sock,
                None => bail!("uTP disabled"),
            };

            // Give TCP priority as it's more mature and simpler.
            if self.enable_tcp {
                // wait until either 1 second has passed or TCP failed.
                tokio::select! {
                    _ = tcp_failed_notify.notified() => {},
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
            }

            let conn = sock.connect(addr).await?;

            debug!(?addr, "connected over uTP");
            Ok(conn)
        };

        tokio::pin!(tcp_connect);
        tokio::pin!(utp_connect);

        let mut tcp_err = None;
        let mut utp_err = None;

        while tcp_err.is_none() || utp_err.is_none() {
            tokio::select! {
                tcp_res = &mut tcp_connect, if tcp_err.is_none() => {
                    match tcp_res {
                        Ok(stream) => {
                            let (r, w) = stream.into_split();
                            return Ok((ConnectionKind::Tcp, Box::new(r), Box::new(w)));
                        },
                        Err(e) => {
                            tcp_err = Some(e);
                            tcp_failed_notify.notify_waiters();
                        }
                    }
                },
                utp_res = &mut utp_connect, if utp_err.is_none() => {
                    match utp_res {
                        Ok(stream) => {
                            let (r, w) = stream.split();
                            return Ok((ConnectionKind::Utp, Box::new(r), Box::new(w)));
                        },
                        Err(e) => {
                            utp_err = Some(e);
                        }
                    }
                },
            };
        }

        match (&tcp_err, &utp_err) {
            (Some(tcp_err), Some(utp_err)) => {
                bail!("error connecting: TCP={tcp_err:#}, uTP={utp_err:#}")
            }
            // this is unreachable really, but who knows
            (_, _) => bail!("bug: tcp_err={tcp_err:?}, utp_err={utp_err:?}"),
        }
    }
}
