//! TCP tunnels through an HTTP proxy with `CONNECT` (RFC 9110 §9.3.6).

use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::TcpStream,
};

use super::{HostPort, ProxyConfig, ProxyError, TargetAddr};

/// The most bytes of response head (status line and headers) read from the proxy.
const MAX_RESPONSE_HEAD: usize = 16 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_REASON_LEN: usize = 100;

/// A TCP tunnel opened by an HTTP proxy. Reading starts with whatever the proxy sent after its
/// response head; everything else goes straight to the underlying stream.
#[derive(Debug)]
pub struct HttpTunnel<S = TcpStream> {
    inner: S,
    buffered: Vec<u8>,
    pos: usize,
}

impl<S> HttpTunnel<S> {
    /// The underlying stream. Bytes the proxy sent after its response head and that were not
    /// read yet are not in it.
    pub fn get_ref(&self) -> &S {
        &self.inner
    }
}

/// Connects to the HTTP proxy and asks it for a tunnel to `target`.
pub async fn http_connect(
    proxy: &ProxyConfig,
    target: TargetAddr<'_>,
) -> Result<HttpTunnel, ProxyError> {
    let stream = TcpStream::connect((proxy.host(), proxy.port()))
        .await
        .map_err(ProxyError::ConnectToProxy)?;
    http_connect_with_stream(stream, proxy, target).await
}

/// Asks the HTTP proxy at the other end of `stream` for a tunnel to `target`.
pub async fn http_connect_with_stream<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    proxy: &ProxyConfig,
    target: TargetAddr<'_>,
) -> Result<HttpTunnel<S>, ProxyError> {
    let authority = match target {
        TargetAddr::Ip(addr) => addr.to_string(),
        TargetAddr::Domain(host, port) => HostPort(host, port).to_string(),
    };
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let Some(username) = proxy.username() {
        let credentials = format!("{username}:{}", proxy.password().unwrap_or_default());
        request.push_str("Proxy-Authorization: Basic ");
        request.push_str(&data_encoding::BASE64.encode(credentials.as_bytes()));
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(ProxyError::Io)?;

    let mut buf = Vec::with_capacity(1024);
    loop {
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut response = httparse::Response::new(&mut headers);
        let head_len = match response.parse(&buf) {
            Ok(httparse::Status::Complete(len)) => len,
            Ok(httparse::Status::Partial) => {
                if buf.len() >= MAX_RESPONSE_HEAD {
                    return Err(ProxyError::Malformed("response head too long"));
                }
                let limit = (MAX_RESPONSE_HEAD - buf.len()) as u64;
                let n = (&mut stream)
                    .take(limit)
                    .read_buf(&mut buf)
                    .await
                    .map_err(ProxyError::Io)?;
                if n == 0 {
                    return Err(ProxyError::Closed);
                }
                continue;
            }
            Err(_) => return Err(ProxyError::Malformed("invalid HTTP response")),
        };
        let status = response
            .code
            .ok_or(ProxyError::Malformed("invalid HTTP response"))?;
        match status {
            // Interim responses come before the final one (RFC 9110 §15.2).
            100 | 102..=199 => {
                buf.drain(..head_len);
                continue;
            }
            200..=299 => {
                buf.drain(..head_len);
                return Ok(HttpTunnel {
                    inner: stream,
                    buffered: buf,
                    pos: 0,
                });
            }
            407 => return Err(ProxyError::HttpAuthenticationRequired),
            status => {
                let reason = response
                    .reason
                    .unwrap_or_default()
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(MAX_REASON_LEN)
                    .collect();
                return Err(ProxyError::HttpStatus { status, reason });
            }
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for HttpTunnel<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Some(rest) = this.buffered.get(this.pos..)
            && !rest.is_empty()
        {
            let n = rest.len().min(buf.remaining());
            buf.put_slice(&rest[..n]);
            this.pos += n;
            if this.pos == this.buffered.len() {
                this.buffered = Vec::new();
                this.pos = 0;
            }
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for HttpTunnel<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    use super::*;

    const TARGET: TargetAddr<'static> = TargetAddr::Ip(SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1)),
        6881,
    ));

    /// Runs a CONNECT against a scripted proxy that answers `answer` and closes, and returns
    /// the request it received, plus the result.
    async fn exchange(
        proxy: &str,
        target: TargetAddr<'_>,
        answer: &'static [u8],
    ) -> (
        String,
        Result<HttpTunnel<tokio::io::DuplexStream>, ProxyError>,
    ) {
        let (client, mut server) = duplex(64 * 1024);
        let proxy = ProxyConfig::parse(proxy).unwrap();
        let server_task = tokio::spawn(async move {
            let mut req = Vec::new();
            while !req.ends_with(b"\r\n\r\n") {
                let mut b = [0u8; 1];
                if server.read(&mut b).await.unwrap() == 0 {
                    break;
                }
                req.push(b[0]);
            }
            server.write_all(answer).await.unwrap();
            String::from_utf8(req).unwrap()
        });
        let res = http_connect_with_stream(client, &proxy, target).await;
        (server_task.await.unwrap(), res)
    }

    #[tokio::test]
    async fn request_without_credentials() {
        let (req, res) = exchange("http://p:3128", TARGET, b"HTTP/1.1 200 OK\r\n\r\n").await;
        res.unwrap();
        assert_eq!(
            req,
            "CONNECT 10.0.0.1:6881 HTTP/1.1\r\nHost: 10.0.0.1:6881\r\n\r\n"
        );
    }

    #[tokio::test]
    async fn request_with_basic_auth_and_ipv6() {
        let target = TargetAddr::Ip("[2001:db8::1]:80".parse().unwrap());
        let (req, res) = exchange(
            "http://Aladdin:open%20sesame@p:3128",
            target,
            b"HTTP/1.0 200 Connection established\r\nVia: x\r\n\r\n",
        )
        .await;
        res.unwrap();
        assert_eq!(
            req,
            "CONNECT [2001:db8::1]:80 HTTP/1.1\r\nHost: [2001:db8::1]:80\r\n\
             Proxy-Authorization: Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==\r\n\r\n"
        );
    }

    #[tokio::test]
    async fn domain_target() {
        let (req, res) = exchange(
            "http://p:3128",
            TargetAddr::Domain("tracker.example", 443),
            b"HTTP/1.1 200 OK\r\n\r\n",
        )
        .await;
        res.unwrap();
        assert!(req.starts_with("CONNECT tracker.example:443 HTTP/1.1\r\n"));
    }

    #[tokio::test]
    async fn bytes_after_the_head_belong_to_the_tunnel() {
        let (_, res) = exchange(
            "http://p:3128",
            TARGET,
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nX: y\r\n\r\n\x13BitTorrent protocol",
        )
        .await;
        let mut tunnel = res.unwrap();
        let mut got = [0u8; 20];
        tunnel.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"\x13BitTorrent protocol");
    }

    #[tokio::test]
    async fn errors() {
        let (_, res) = exchange(
            "http://u:p@p:3128",
            TARGET,
            b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n",
        )
        .await;
        assert!(matches!(res, Err(ProxyError::HttpAuthenticationRequired)));

        let (_, res) = exchange("http://p:3128", TARGET, b"HTTP/1.1 403 Forbidden\r\n\r\n").await;
        match res {
            Err(ProxyError::HttpStatus { status, reason }) => {
                assert_eq!(status, 403);
                assert_eq!(reason, "Forbidden");
            }
            other => panic!("unexpected {other:?}"),
        }

        let (_, res) = exchange("http://p:3128", TARGET, b"SSH-2.0-OpenSSH\r\n\r\n").await;
        assert!(matches!(res, Err(ProxyError::Malformed(_))));
    }

    #[tokio::test]
    async fn closed_before_answer() {
        let (client, server) = duplex(1024);
        drop(server);
        let proxy = ProxyConfig::parse("http://p:3128").unwrap();
        let res = http_connect_with_stream(client, &proxy, TARGET).await;
        assert!(matches!(res, Err(ProxyError::Io(_) | ProxyError::Closed)));
    }

    #[tokio::test]
    async fn endless_head_is_bounded() {
        let (client, mut server) = duplex(64 * 1024);
        let proxy = ProxyConfig::parse("http://p:3128").unwrap();
        tokio::spawn(async move {
            let mut b = [0u8; 1024];
            let _ = server.read(&mut b).await;
            let _ = server.write_all(b"HTTP/1.1 200 OK\r\n").await;
            let line = [b'a'; 1000];
            loop {
                if server.write_all(&line).await.is_err() {
                    return;
                }
            }
        });
        let res = http_connect_with_stream(client, &proxy, TARGET).await;
        assert!(matches!(res, Err(ProxyError::Malformed(_))), "{res:?}");
    }
}
