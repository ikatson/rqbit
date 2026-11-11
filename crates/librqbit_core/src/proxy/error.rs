use super::ProxyKind;

/// Why talking through a proxy failed. The messages never contain the proxy password.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProxyError {
    /// The TCP connection to the proxy itself could not be made.
    #[error("error connecting to the proxy: {0}")]
    ConnectToProxy(#[source] std::io::Error),
    /// Reading from or writing to the proxy failed.
    #[error("error talking to the proxy: {0}")]
    Io(#[source] std::io::Error),
    /// The proxy closed the connection before answering.
    #[error("the proxy closed the connection")]
    Closed,
    /// The proxy answered something that is not valid for the protocol.
    #[error("malformed answer from the proxy: {0}")]
    Malformed(&'static str),
    /// The HTTP proxy wants credentials, or rejected the ones it got (status 407).
    #[error("the HTTP proxy requires authentication (407 Proxy Authentication Required)")]
    HttpAuthenticationRequired,
    /// The HTTP proxy refused to open the tunnel.
    #[error("the HTTP proxy refused the tunnel: {status} {reason}")]
    HttpStatus {
        /// The HTTP status code.
        status: u16,
        /// The reason phrase, shortened and stripped of control characters.
        reason: String,
    },
    /// The SOCKS5 proxy accepts none of the authentication methods we can use.
    #[error("the SOCKS5 proxy accepts none of the offered authentication methods")]
    Socks5NoAcceptableMethod,
    /// The SOCKS5 proxy rejected the user name and password.
    #[error("the SOCKS5 proxy rejected the user name or password")]
    Socks5AuthenticationFailed,
    /// The SOCKS5 proxy refused the request (RFC 1928 §6 reply code).
    #[error("the SOCKS5 proxy refused the request: {}", socks5_reply_text(*.0))]
    Socks5Refused(u8),
    /// The proxy can't relay UDP (only SOCKS5 can).
    #[error("{0} proxies cannot relay UDP")]
    UdpNotSupported(ProxyKind),
    /// The UDP association with the proxy is not established (the proxy is down, or the
    /// association is being rebuilt).
    #[error("no UDP association with the proxy: {0}")]
    UdpAssociationDown(String),
    /// The UDP association was shut down (its owner was dropped or cancelled).
    #[error("the UDP association with the proxy was shut down")]
    UdpAssociationShutDown,
    /// A domain name longer than 255 bytes can't be sent to a SOCKS5 proxy.
    #[error("the domain name is too long for SOCKS5")]
    DomainTooLong,
    /// The datagram plus the SOCKS5 UDP header doesn't fit in one UDP datagram.
    #[error("datagram too large to relay through the proxy")]
    DatagramTooLarge,
    /// A host name that can't be put in a request as it is: an HTTP `CONNECT` target must be
    /// ASCII letters, digits, `-`, `.` and `_` (international names in their `xn--` form).
    #[error("invalid target host name")]
    InvalidTargetHost,
}

/// RFC 1928 §6.
fn socks5_reply_text(code: u8) -> String {
    let text = match code {
        1 => "general SOCKS server failure",
        2 => "connection not allowed by ruleset",
        3 => "network unreachable",
        4 => "host unreachable",
        5 => "connection refused",
        6 => "TTL expired",
        7 => "command not supported",
        8 => "address type not supported",
        other => return format!("unknown reply code {other}"),
    };
    text.to_owned()
}
