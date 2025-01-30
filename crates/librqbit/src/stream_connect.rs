use std::net::SocketAddr;

use anyhow::Context;

#[derive(Debug, Clone)]
pub(crate) struct SocksProxyConfig {
    pub host: String,
    pub port: u16,
    pub username_password: Option<(String, String)>,
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

#[derive(Debug, Default)]
pub(crate) struct StreamConnector {
    proxy_config: Option<SocksProxyConfig>,
}

impl From<Option<SocksProxyConfig>> for StreamConnector {
    fn from(proxy_config: Option<SocksProxyConfig>) -> Self {
        Self { proxy_config }
    }
}

impl StreamConnector {
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

        let (r, w) = tokio::net::TcpStream::connect(addr)
            .await
            .context("error connecting")?
            .into_split();
        Ok((Box::new(r), Box::new(w)))
    }
}
