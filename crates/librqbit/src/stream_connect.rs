use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{bail, Context};
use librqbit_utp::UtpSocketUdp;
use tracing::debug;

use crate::{
    type_aliases::{BoxAsyncRead, BoxAsyncWrite},
    PeerConnectionOptions,
};

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
        impl tokio::io::AsyncRead + Unpin,
        impl tokio::io::AsyncWrite + Unpin,
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
            utp_socket: config.utp_socket,
        })
    }

    pub async fn connect(&self, addr: SocketAddr) -> anyhow::Result<(BoxAsyncRead, BoxAsyncWrite)> {
        if let Some(proxy) = self.proxy_config.as_ref() {
            let (r, w) = proxy.connect(addr).await?;
            debug!(?addr, "connected through SOCKS5");
            return Ok((Box::new(r), Box::new(w)));
        }

        // Try to connect over TCP first. If in 1 second we haven't connected, try uTP also (if configured).
        // Whoever connects first wins.

        let tcp_connect = async {
            if !self.enable_tcp {
                bail!("TCP outgoing connections disabled");
            }
            let conn = tokio::net::TcpStream::connect(addr)
                .await
                .context("error connecting over TCP");
            debug!(?addr, "connected over TCP");
            conn
        };

        let utp_connect = async {
            let sock = match self.utp_socket.as_ref() {
                Some(sock) => sock,
                None => bail!("uTP disabled"),
            };

            // Give TCP priority as it's more mature and simpler.
            if self.enable_tcp {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }

            let conn = sock
                .connect(addr)
                .await
                .context("error connecting over uTP");

            debug!(?addr, "connected over uTP");
            conn
        };

        tokio::pin!(tcp_connect);
        tokio::pin!(utp_connect);

        let mut tcp_failed = false;
        let mut utp_failed = false;

        while !tcp_failed || !utp_failed {
            tokio::select! {
                tcp_res = &mut tcp_connect, if !tcp_failed => {
                    match tcp_res {
                        Ok(stream) => {
                            let (r, w) = stream.into_split();
                            return Ok((Box::new(r), Box::new(w)));
                        },
                        Err(e) => {
                            debug!(addr=?addr, "error connecting over TCP: {e:#}");
                            tcp_failed = true;
                        }
                    }
                },
                utp_res = &mut utp_connect, if !utp_failed => {
                    match utp_res {
                        Ok(stream) => {
                            let (r, w) = stream.split();
                            return Ok((Box::new(r), Box::new(w)));
                        },
                        Err(e) => {
                            debug!(addr=?addr, "error connecting over uTP: {e:#}");
                            utp_failed = true;
                        }
                    }
                },
            };
        }

        bail!("can't connect to {addr}")
    }
}
