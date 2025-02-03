use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{bail, Context};
use librqbit_utp::UtpSocketUdp;
use tracing::debug;

#[derive(Debug, Clone)]
pub(crate) struct SocksProxyConfig {
    pub host: String,
    pub port: u16,
    pub username_password: Option<(String, String)>,
}

#[derive(Default, Debug, Clone)]
pub(crate) struct StreamConnectorConfig {
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
    utp_socket: Option<Arc<librqbit_utp::UtpSocketUdp>>,
}

impl StreamConnector {
    pub async fn new(config: StreamConnectorConfig) -> anyhow::Result<Self> {
        Ok(Self {
            proxy_config: config.socks_proxy_config,
            utp_socket: config.utp_socket,
        })
    }

    pub async fn connect(
        &self,
        addr: SocketAddr,
    ) -> anyhow::Result<(
        Box<dyn tokio::io::AsyncRead + Send + Unpin>,
        Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    )> {
        if let Some(proxy) = self.proxy_config.as_ref() {
            let (r, w) = proxy.connect(addr).await?;
            return Ok((Box::new(r), Box::new(w)));
        }

        // Try to connect over TCP first. If in 1 second we haven't connected, try uTP also (if configured).
        // Whoever connects first wins.

        let tcp_connect = async {
            tokio::net::TcpStream::connect(addr)
                .await
                .context("error connecting over TCP")
        };

        let utp_connect = async {
            let sock = match self.utp_socket.as_ref() {
                Some(sock) => sock,
                None => bail!("uTP disabled"),
            };

            tokio::time::sleep(Duration::from_secs(1)).await;
            sock.connect(addr)
                .await
                .context("error connecting over uTP")
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
