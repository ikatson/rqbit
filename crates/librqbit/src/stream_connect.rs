use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, bail};
use librqbit_dualstack_sockets::ConnectOpts;
use librqbit_utp::{BindDevice, UtpSocketUdp};
use serde::Serialize;

use crate::{
    Error, PeerConnectionOptions, Result, mse::{MseMode, OutgoingOutcome},
    peer_connection::with_timeout,
    read_buf::ReadBuf,
    type_aliases::{BoxAsyncReadVectored, BoxAsyncWrite},
    vectored_traits::AsyncReadVectoredIntoCompat,
};
use peer_binary_protocol::Handshake;
use tokio::io::AsyncWriteExt;
use tracing::{debug, warn};

#[derive(Debug, Clone, Copy, Serialize)]
pub enum ConnectionKind {
    #[serde(rename = "tcp")]
    Tcp,
    #[serde(rename = "utp")]
    Utp,
    #[serde(rename = "socks")]
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

/// Result of connecting and (optionally) performing an MSE handshake.
pub struct OutgoingHandshake {
    pub kind: ConnectionKind,
    pub read: BoxAsyncReadVectored,
    pub write: BoxAsyncWrite,
    /// True when the MSE handshake succeeded and consumed the BT handshake as
    /// its initial payload (IA); the caller must not write a plaintext
    /// handshake in that case.
    #[allow(dead_code)] // read by the MSE fallback tests
    pub mse_applied: bool,
}

/// Result of accepting a connection and reading its (plaintext or MSE) BT
/// handshake.
pub(crate) struct IncomingHandshake {
    pub handshake: Handshake,
    pub read: BoxAsyncReadVectored,
    pub write: BoxAsyncWrite,
    pub read_buf: ReadBuf,
    /// True when the handshake arrived over an MSE-encrypted stream.
    pub encrypted: bool,
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
    pub bind_device: Option<BindDevice>,
    pub ipv4_only: bool,
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
    ) -> tokio_socks::Result<(
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
            .await?
        } else {
            tokio_socks::tcp::Socks5Stream::connect(proxy_addr, addr).await?
        };

        Ok(tokio::io::split(stream))
    }
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
    tcp PerFamilyAtomic PerFamilySnapshot,
    utp PerFamilyAtomic PerFamilySnapshot
]);

pub(crate) struct StreamConnector {
    proxy_config: Option<SocksProxyConfig>,
    enable_tcp: bool,
    bind_device: Option<BindDevice>,
    utp_socket: Option<Arc<librqbit_utp::UtpSocketUdp>>,
    stats: ConnectStatsAtomic,
    ipv4_only: bool,
    // Pre-built connections consumed by `connect()` in test builds, in order.
    #[cfg(test)]
    test_connections:
        parking_lot::Mutex<std::collections::VecDeque<(BoxAsyncReadVectored, BoxAsyncWrite)>>,
}

impl std::fmt::Debug for StreamConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamConnector")
            .field("proxy_config", &self.proxy_config)
            .field("enable_tcp", &self.enable_tcp)
            .field("bind_device", &self.bind_device)
            .field("utp_socket", &self.utp_socket)
            .field("ipv4_only", &self.ipv4_only)
            .finish()
    }
}

impl StreamConnector {
    #[cfg(test)]
    pub fn with_test_connections(conns: Vec<(BoxAsyncReadVectored, BoxAsyncWrite)>) -> Self {
        Self {
            proxy_config: None,
            enable_tcp: true,
            utp_socket: None,
            bind_device: None,
            stats: Default::default(),
            ipv4_only: false,
            test_connections: parking_lot::Mutex::new(conns.into()),
        }
    }

    #[cfg(test)]
    pub fn remaining_test_connections(&self) -> anyhow::Result<usize> {
        Ok(self.test_connections.lock().len())
    }

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
            bind_device: config.bind_device,
            stats: Default::default(),
            ipv4_only: config.ipv4_only,
            #[cfg(test)]
            test_connections: Default::default(),
        })
    }

    fn get_stat(&self, kind: ConnectionKind, is_v6: bool) -> &SingleStatAtomic {
        let stat = match kind {
            ConnectionKind::Tcp => &self.stats.tcp,
            ConnectionKind::Utp => &self.stats.utp,
            ConnectionKind::Socks => &self.stats.socks,
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
        #[cfg(test)]
        if let Some((r, w)) = self.test_connections.lock().pop_front() {
            return Ok((ConnectionKind::Tcp, r, w));
        }

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

        if let Some(proxy) = self.proxy_config.as_ref() {
            let (r, w) = self
                .with_stat(ConnectionKind::Socks, addr.is_ipv6(), proxy.connect(addr))
                .await?;
            debug!(?addr, "connected through SOCKS5");
            return Ok((
                ConnectionKind::Socks,
                Box::new(r.into_vectored_compat()),
                Box::new(w),
            ));
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

    /// Connect to `addr` and complete the BitTorrent handshake, optionally
    /// wrapping the stream in MSE (Message Stream Encryption) per `mse_mode`.
    ///
    /// Serializes the 68-byte BT handshake once; when MSE succeeds it is
    /// consumed as the MSE initial payload (IA), otherwise it is written
    /// plaintext. On MSE failure or a plaintext peer, redials plaintext on a
    /// fresh connection (never reuses the MSE-polluted stream). `Forced` mode
    /// errors instead of downgrading. The returned streams are always
    /// handshake-complete: the caller must not write a BT handshake again.
    pub async fn connect_with_handshake(
        &self,
        addr: SocketAddr,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        connect_timeout: Duration,
        rwtimeout: Duration,
        mse_mode: MseMode,
    ) -> Result<OutgoingHandshake> {
        let (ckind, read, write) =
            with_timeout("connecting", connect_timeout, self.connect(addr)).await?;

        let handshake = Handshake::new(
            librqbit_core::hash_id::Id20::new(*info_hash),
            librqbit_core::hash_id::Id20::new(*peer_id),
        );
        let mut write_buf = [0u8; 68];
        let hsz = handshake.serialize_unchecked_len(&mut write_buf);

        if mse_mode != MseMode::Disabled {
            let mse_outcome = match tokio::time::timeout(
                rwtimeout,
                crate::mse::outgoing(read, write, info_hash, &write_buf),
            )
            .await
            {
                Ok(Ok(outcome)) => outcome,
                Ok(Err(e)) => {
                    if mse_mode == MseMode::Forced {
                        warn!(?addr, "MSE forced but handshake failed, dropping peer: {e:#}");
                        return Err(Error::MseForced);
                    }
                    debug!(?addr, "MSE handshake failed, redialing plaintext: {e:#}");
                    return self
                        .redial_plaintext(addr, connect_timeout, rwtimeout, &write_buf, hsz)
                        .await;
                }
                Err(_elapsed) => {
                    if mse_mode == MseMode::Forced {
                        warn!(?addr, "MSE forced but handshake timed out, dropping peer");
                        return Err(Error::MseForced);
                    }
                    debug!(?addr, "MSE handshake timed out, redialing plaintext");
                    return self
                        .redial_plaintext(addr, connect_timeout, rwtimeout, &write_buf, hsz)
                        .await;
                }
            };

            match mse_outcome {
                OutgoingOutcome::Encrypted(r, w) => {
                    debug!(?addr, "MSE handshake succeeded, RC4 established");
                    return Ok(OutgoingHandshake {
                        kind: ckind,
                        read: Box::new(r.into_vectored_compat()),
                        write: Box::new(w),
                        mse_applied: true,
                    });
                }
                OutgoingOutcome::PlaintextPeer => {
                    if mse_mode == MseMode::Forced {
                        warn!(?addr, "MSE forced but peer answered plaintext, dropping peer");
                        return Err(Error::MseForced);
                    }
                    debug!(?addr, "peer answered plaintext, redialing plaintext");
                    return self
                        .redial_plaintext(addr, connect_timeout, rwtimeout, &write_buf, hsz)
                        .await;
                }
            }
        }

        Self::finish_plaintext_handshake(ckind, read, write, &write_buf, hsz, rwtimeout).await
    }

    /// Redial a fresh plaintext connection after MSE failed or the peer
    /// answered plaintext, writing the serialized BT handshake.
    async fn redial_plaintext(
        &self,
        addr: SocketAddr,
        connect_timeout: Duration,
        rwtimeout: Duration,
        write_buf: &[u8; 68],
        hsz: usize,
    ) -> Result<OutgoingHandshake> {
        let (nk, nr, nw) =
            with_timeout("connecting", connect_timeout, self.connect(addr)).await?;
        Self::finish_plaintext_handshake(nk, nr, nw, write_buf, hsz, rwtimeout).await
    }

    /// Write the serialized BT handshake on a plaintext stream and wrap it.
    async fn finish_plaintext_handshake(
        ckind: ConnectionKind,
        read: BoxAsyncReadVectored,
        write: BoxAsyncWrite,
        write_buf: &[u8; 68],
        hsz: usize,
        rwtimeout: Duration,
    ) -> Result<OutgoingHandshake> {
        use futures::TryFutureExt;
        let mut write = write;
        with_timeout(
            "writing",
            rwtimeout,
            write.write_all(&write_buf[..hsz]).map_err(Error::WriteHandshake),
        )
        .await?;
        Ok(OutgoingHandshake { kind: ckind, read, write, mse_applied: false })
    }
}

/// Accept an incoming connection and read its BT handshake, optionally
/// attempting the MSE (Message Stream Encryption) handshake per `mse_mode`.
///
/// With MSE enabled, probes the peer for a plaintext BT prefix; if it is a
/// plaintext peer it is accepted unless `mse_mode == Forced`, otherwise the
/// MSE acceptor runs (resolving the obfuscated info hash via `lookup`). The
/// returned streams are ready for post-handshake traffic.
pub(crate) async fn accept_with_handshake<F>(
    addr: SocketAddr,
    reader: BoxAsyncReadVectored,
    writer: BoxAsyncWrite,
    rwtimeout: Duration,
    mse_mode: MseMode,
    lookup: F,
) -> anyhow::Result<IncomingHandshake>
where
    F: Fn(&[u8; 20]) -> Option<[u8; 20]>,
{
    use crate::mse::IncomingOutcome;
    use tokio::io::AsyncReadExt;

    if mse_mode == MseMode::Disabled {
        let mut read_buf = ReadBuf::new();
        let mut reader = reader;
        let h = read_buf
            .read_handshake(&mut reader, rwtimeout)
            .await
            .context("error reading handshake")?;
        return Ok(IncomingHandshake {
            handshake: h,
            read: reader,
            write: writer,
            read_buf,
            encrypted: false,
        });
    }

    let incoming = crate::mse::incoming(reader, writer, lookup);
    let incoming = tokio::time::timeout(rwtimeout, incoming)
        .await
        .context("MSE incoming handshake timed out")??;

    match incoming {
        IncomingOutcome::Encrypted {
            read,
            write,
            handshake_bytes,
            info_hash,
        } => {
            let (h, _size) = Handshake::deserialize(&handshake_bytes[..])
                .map_err(|e| anyhow::anyhow!("error deserializing MSE handshake: {e:?}"))?;
            if h.info_hash.0 != info_hash {
                bail!("MSE handshake info hash does not match SKEY");
            }
            Ok(IncomingHandshake {
                handshake: h,
                read: Box::new(read.into_vectored_compat()),
                write: Box::new(write),
                read_buf: ReadBuf::new(),
                encrypted: true,
            })
        }
        IncomingOutcome::Plaintext { read, write } => {
            if mse_mode == MseMode::Forced {
                warn!(?addr, "MSE forced, rejecting plaintext connection");
                bail!("MSE is forced, rejecting plaintext connection from {addr}");
            }
            // 9.0's `ReadBuf::read_handshake` performs a single `read()`
            // before deserializing; a `PrefixReader` replaying the 20
            // consumed prefix bytes would satisfy it with only those bytes.
            // Read the full 68-byte handshake ourselves (read_exact loops),
            // mirroring the Encrypted branch.
            let mut handshake_bytes = [0u8; 68];
            let mut read = read;
            read.read_exact(&mut handshake_bytes)
                .await
                .context("error reading fragmented plaintext handshake")?;
            let (h, _size) = Handshake::deserialize(&handshake_bytes[..]).map_err(|e| {
                anyhow::anyhow!("error deserializing plaintext handshake: {e:?}")
            })?;
            Ok(IncomingHandshake {
                handshake: h,
                read: Box::new(read.into_vectored_compat()),
                write: Box::new(write),
                read_buf: ReadBuf::new(),
                encrypted: false,
            })
        }
    }
}
