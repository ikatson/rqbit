use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::bail;
use librqbit_core::proxy::{ProxyConfig, ProxyKind, TargetAddr, http_connect};
use librqbit_dualstack_sockets::ConnectOpts;
use librqbit_utp::{BindDevice, UtpSocketUdp};
use serde::Serialize;
use tracing::debug;

use crate::{
    Error, PeerConnectionOptions, Result,
    type_aliases::{BoxAsyncReadVectored, BoxAsyncWrite},
    vectored_traits::AsyncReadVectoredIntoCompat,
};

/// The transport of a peer connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ConnectionKind {
    /// Direct TCP.
    #[serde(rename = "tcp")]
    Tcp,
    /// Direct uTP.
    #[serde(rename = "utp")]
    Utp,
    /// TCP through a SOCKS5 or SOCKS4 proxy.
    #[serde(rename = "socks")]
    Socks,
    /// TCP through an HTTP proxy (`CONNECT` tunnel).
    #[serde(rename = "http_proxy")]
    HttpProxy,
}

impl std::fmt::Display for ConnectionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionKind::Tcp => f.write_str("tcp"),
            ConnectionKind::Utp => f.write_str("uTP"),
            ConnectionKind::Socks => f.write_str("socks"),
            ConnectionKind::HttpProxy => f.write_str("http proxy"),
        }
    }
}

/// How a session makes outgoing connections.
pub struct ConnectionOptions {
    /// A proxy for the traffic of the session, as a URL: `socks5://`, `socks5h://` (the proxy
    /// resolves host names), `socks4://`, `socks4a://` or `http://` (`CONNECT` tunnels), with
    /// optional `user:password@` (the user id alone for SOCKS4), e.g.
    /// `socks5h://user:secret@proxy.example:1080`.
    ///
    /// Tracker traffic and the other HTTP requests of the session always go through it. Peer
    /// connections and the DHT do too unless [`Self::proxy_peer_connections`] is false. What
    /// can't go through a proxy is turned off instead of being sent directly: UDP trackers and
    /// the DHT behind a SOCKS4 or HTTP proxy (only SOCKS5 relays UDP), and outgoing uTP behind
    /// any proxy (TCP through the proxy is used instead). Incoming connections are not affected
    /// (see [`crate::ListenerOptions`]), but a session behind a proxy is normally not reachable
    /// from outside. Port mapping (UPnP / NAT-PMP) and local service discovery only talk to the
    /// local network and stay direct; a DHT behind the proxy has no port of its own to map.
    /// Behind `socks5h` host names are never looked up locally: DHT bootstrap nodes given by
    /// name are skipped, UDP trackers are sent to the proxy by name.
    pub proxy_url: Option<String>,
    /// Whether peer connections and the DHT go through [`Self::proxy_url`] (default true). When
    /// false, only tracker traffic and the HTTP requests use the proxy; peers, the DHT and
    /// outgoing uTP work directly as without a proxy.
    pub proxy_peer_connections: bool,
    /// Whether outgoing TCP connections are made (default true).
    pub enable_tcp: bool,
    /// Options of the peer connections.
    pub peer_opts: Option<PeerConnectionOptions>,
}

impl Default for ConnectionOptions {
    fn default() -> Self {
        Self {
            enable_tcp: true,
            proxy_url: None,
            proxy_peer_connections: true,
            peer_opts: None,
        }
    }
}

#[derive(Default, Debug, Clone)]
pub(crate) struct StreamConnectorArgs {
    pub enable_tcp: bool,
    pub proxy: Option<ProxyConfig>,
    pub utp_socket: Option<Arc<UtpSocketUdp>>,
    pub bind_device: Option<BindDevice>,
    pub ipv4_only: bool,
}

/// Opens a TCP connection to `addr` through the proxy.
async fn proxy_connect(
    proxy: &ProxyConfig,
    addr: SocketAddr,
) -> Result<(BoxAsyncReadVectored, BoxAsyncWrite)> {
    fn boxed(
        stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
    ) -> (BoxAsyncReadVectored, BoxAsyncWrite) {
        let (r, w) = tokio::io::split(stream);
        (Box::new(r.into_vectored_compat()), Box::new(w))
    }

    let proxy_addr = (proxy.host(), proxy.port());
    Ok(match proxy.kind() {
        ProxyKind::Socks5 | ProxyKind::Socks5h => {
            boxed(match (proxy.username(), proxy.password()) {
                (Some(username), Some(password)) => {
                    tokio_socks::tcp::Socks5Stream::connect_with_password(
                        proxy_addr, addr, username, password,
                    )
                    .await?
                }
                _ => tokio_socks::tcp::Socks5Stream::connect(proxy_addr, addr).await?,
            })
        }
        ProxyKind::Socks4 | ProxyKind::Socks4a => boxed(match proxy.username() {
            Some(user_id) => {
                tokio_socks::tcp::Socks4Stream::connect_with_userid(proxy_addr, addr, user_id)
                    .await?
            }
            None => tokio_socks::tcp::Socks4Stream::connect(proxy_addr, addr).await?,
        }),
        ProxyKind::Http => boxed(http_connect(proxy, TargetAddr::Ip(addr)).await?),
    })
}

gen_stats!(SingleStatAtomic SingleStatSnapshot, [
    attempts u64,
    successes u64,
    errors u64
], []);
gen_stats!(PerFamilyAtomic PerFamilySnapshot, [], [
    v4 SingleStatAtomic SingleStatSnapshot,
    v6 SingleStatAtomic SingleStatSnapshot
]);
gen_stats!(ConnectStatsAtomic ConnectStatsSnapshot, [], [
    socks PerFamilyAtomic PerFamilySnapshot,
    http_proxy PerFamilyAtomic PerFamilySnapshot,
    tcp PerFamilyAtomic PerFamilySnapshot,
    utp PerFamilyAtomic PerFamilySnapshot
]);

#[derive(Debug)]
pub(crate) struct StreamConnector {
    proxy_config: Option<ProxyConfig>,
    enable_tcp: bool,
    bind_device: Option<BindDevice>,
    utp_socket: Option<Arc<librqbit_utp::UtpSocketUdp>>,
    stats: ConnectStatsAtomic,
    ipv4_only: bool,
}

impl StreamConnector {
    pub async fn new(config: StreamConnectorArgs) -> anyhow::Result<Self> {
        #[allow(clippy::single_match)]
        match (
            config.proxy.is_some(),
            config.enable_tcp,
            config.utp_socket.is_some(),
        ) {
            (false, false, false) => {
                bail!("no way to connect to peers, enable TCP, uTP or a proxy")
            }
            _ => {
                // TODO: maybe validate other combinations. For now there's no way to disable TCP
            }
        }

        Ok(Self {
            proxy_config: config.proxy,
            enable_tcp: config.enable_tcp,
            utp_socket: config.utp_socket,
            bind_device: config.bind_device,
            stats: Default::default(),
            ipv4_only: config.ipv4_only,
        })
    }

    fn get_stat(&self, kind: ConnectionKind, is_v6: bool) -> &SingleStatAtomic {
        let stat = match kind {
            ConnectionKind::Tcp => &self.stats.tcp,
            ConnectionKind::Utp => &self.stats.utp,
            ConnectionKind::Socks => &self.stats.socks,
            ConnectionKind::HttpProxy => &self.stats.http_proxy,
        };
        if is_v6 { &stat.v6 } else { &stat.v4 }
    }

    async fn with_stat<R, E>(
        &self,
        kind: ConnectionKind,
        is_v6: bool,
        fut: impl Future<Output = std::result::Result<R, E>>,
    ) -> std::result::Result<R, E> {
        let stat = self.get_stat(kind, is_v6);
        stat.attempts(1);
        fut.await
            .inspect(|_| stat.successes(1))
            .inspect_err(|_| stat.errors(1))
    }

    async fn tcp_connect(
        &self,
        addr: SocketAddr,
    ) -> librqbit_dualstack_sockets::Result<tokio::net::TcpStream> {
        self.with_stat(
            ConnectionKind::Tcp,
            addr.is_ipv6(),
            librqbit_dualstack_sockets::tcp_connect(
                addr,
                ConnectOpts {
                    // Setting source port doesn't work with cloudflare warp on linux
                    // source_port: self.tcp_source_port,
                    source_port: None,
                    bind_device: self.bind_device.as_ref(),
                },
            ),
        )
        .await
    }

    pub fn stats(&self) -> &ConnectStatsAtomic {
        &self.stats
    }

    pub async fn connect(
        &self,
        addr: SocketAddr,
    ) -> Result<(ConnectionKind, BoxAsyncReadVectored, BoxAsyncWrite)> {
        if addr.port() == 0 {
            return Err(Error::Anyhow(anyhow::anyhow!(
                "invalid peer address (port 0): {}",
                addr
            )));
        }

        if self.ipv4_only && addr.is_ipv6() {
            return Err(Error::Anyhow(anyhow::anyhow!(
                "ipv6 disabled, skipping connection to {}",
                addr
            )));
        }

        // Behind a proxy every outgoing connection is TCP through it: uTP can't be relayed (it
        // would go out directly), and a failure is the result, never a direct retry.
        if let Some(proxy) = self.proxy_config.as_ref() {
            let kind = match proxy.kind() {
                ProxyKind::Http => ConnectionKind::HttpProxy,
                _ => ConnectionKind::Socks,
            };
            let (r, w) = self
                .with_stat(kind, addr.is_ipv6(), proxy_connect(proxy, addr))
                .await?;
            debug!(?addr, proxy = %proxy.kind(), "connected through proxy");
            return Ok((kind, r, w));
        }

        // Try to connect over TCP first. If in 1 second we haven't connected, try uTP also (if configured).
        // Whoever connects first wins.
        let tcp_connect = async {
            if !self.enable_tcp {
                return Ok(None);
            }
            let conn = self.tcp_connect(addr).await?;
            debug!(?addr, "connected over TCP");
            Ok::<_, librqbit_dualstack_sockets::Error>(Some(conn))
        };

        let tcp_failed_notify = tokio::sync::Notify::new();

        let utp_connect = async {
            let sock = match self.utp_socket.as_ref() {
                Some(sock) => sock,
                None => return Ok(None),
            };

            // Give TCP priority as it's more mature and simpler.
            if self.enable_tcp {
                // wait until either 1 second has passed or TCP failed.
                tokio::select! {
                    _ = tcp_failed_notify.notified() => {},
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
            }

            let conn = self
                .with_stat(ConnectionKind::Utp, addr.is_ipv6(), sock.connect(addr))
                .await?;

            debug!(?addr, "connected over uTP");
            Ok(Some(conn))
        };

        tokio::pin!(tcp_connect);
        tokio::pin!(utp_connect);

        let mut tcp_err: Option<Option<librqbit_dualstack_sockets::Error>> = None;
        let mut utp_err: Option<Option<librqbit_utp::Error>> = None;

        // wait until all fail, or one succeeds.
        loop {
            if let (Some(tcp), Some(utp)) = (tcp_err.as_mut(), utp_err.as_mut()) {
                match (tcp.take(), utp.take()) {
                    (Some(tcp), Some(utp)) => return Err(Error::Connect { tcp, utp }),
                    (Some(tcp), None) => return Err(Error::TcpConnect(tcp)),
                    (None, Some(utp)) => return Err(Error::UtpConnect(utp)),
                    (None, None) => return Err(Error::ConnectDisabled),
                }
            }
            tokio::select! {
                tcp_res = &mut tcp_connect, if tcp_err.is_none() => {
                    match tcp_res {
                        Ok(Some(stream)) => {
                            let (r, w) = stream.into_split();
                            return Ok((ConnectionKind::Tcp, Box::new(r), Box::new(w)));
                        },
                        Ok(None) => {
                            tcp_err = Some(None);
                            tcp_failed_notify.notify_waiters();
                        }
                        Err(e) => {
                            tcp_err = Some(Some(e));
                            tcp_failed_notify.notify_waiters();
                        }
                    }
                },
                utp_res = &mut utp_connect, if utp_err.is_none() => {
                    match utp_res {
                        Ok(Some(stream)) => {
                            let (r, w) = stream.split();
                            return Ok((ConnectionKind::Utp, Box::new(r), Box::new(w)));
                        },
                        Ok(None) => {
                            utp_err = Some(None);
                        }
                        Err(e) => {
                            utp_err = Some(Some(e));
                        }
                    }
                },
            };
        }
    }
}
