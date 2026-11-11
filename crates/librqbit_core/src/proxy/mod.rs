//! Proxy settings shared by every part of a BitTorrent client that talks to the network.
//!
//! A proxy is given as a URL such as `socks5h://user:secret@proxy.example:1080`. [`ProxyConfig`]
//! parses it once, so the peer connector, the HTTP client, the UDP tracker client and the DHT all
//! read the same validated value. Neither its `Debug` nor its `Display` output, nor any
//! [`ProxyUrlError`], contains the password.
//!
//! Supported schemes:
//!
//! | scheme | protocol | UDP | host names resolved by |
//! |---|---|---|---|
//! | `socks5` | SOCKS5 (RFC 1928, RFC 1929 for `user:pass@`) | UDP ASSOCIATE | this host |
//! | `socks5h` | SOCKS5 | UDP ASSOCIATE | the proxy |
//! | `socks4` | SOCKS4 (`user@` is the user id) | no | this host |
//! | `socks4a` | SOCKS4a | no | the proxy |
//! | `http` | HTTP `CONNECT` tunnels (RFC 9110 §9.3.6, `user:pass@` sent as Basic) | no | the proxy |
//!
//! `https` proxies (TLS to the proxy itself) are not supported.

use std::{fmt, net::SocketAddr, str::FromStr};

mod error;
mod http_connect;
mod socks5_udp;

pub use error::ProxyError;
pub use http_connect::{HttpTunnel, http_connect, http_connect_with_stream};
pub use socks5_udp::{
    MAX_UDP_PAYLOAD, SOCKS5_UDP_HEADER_MAX, SOCKS5_UDP_HEADER_MIN, Socks5UdpAssociation,
};

#[cfg(any(test, feature = "test-util"))]
pub mod test_util;

/// The protocol spoken to a proxy, chosen by the scheme of the proxy URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProxyKind {
    /// `socks5://`: SOCKS5; host names are resolved locally before asking the proxy.
    Socks5,
    /// `socks5h://`: SOCKS5; host names are sent to the proxy, which resolves them.
    Socks5h,
    /// `socks4://`: SOCKS4; TCP to IPv4 destinations only, host names are resolved locally.
    Socks4,
    /// `socks4a://`: SOCKS4a; TCP only, host names are sent to the proxy.
    Socks4a,
    /// `http://`: an HTTP proxy used through `CONNECT` tunnels; TCP only.
    Http,
}

impl ProxyKind {
    /// The URL scheme of this kind of proxy.
    pub fn scheme(self) -> &'static str {
        match self {
            ProxyKind::Socks5 => "socks5",
            ProxyKind::Socks5h => "socks5h",
            ProxyKind::Socks4 => "socks4",
            ProxyKind::Socks4a => "socks4a",
            ProxyKind::Http => "http",
        }
    }

    /// Whether the proxy can relay UDP datagrams (SOCKS5 UDP ASSOCIATE). The other kinds only
    /// carry TCP, so UDP traffic (UDP trackers, the DHT) can't go through them.
    pub fn can_relay_udp(self) -> bool {
        matches!(self, ProxyKind::Socks5 | ProxyKind::Socks5h)
    }

    /// Whether host names are given to the proxy to resolve, instead of being resolved locally.
    pub fn resolves_hostnames(self) -> bool {
        matches!(
            self,
            ProxyKind::Socks5h | ProxyKind::Socks4a | ProxyKind::Http
        )
    }
}

impl fmt::Display for ProxyKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.scheme())
    }
}

/// Why a proxy URL was rejected. The messages never contain the password.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProxyUrlError {
    /// The text is not a URL.
    #[error("invalid proxy URL: {0}")]
    Invalid(url::ParseError),
    /// The scheme is none of the supported ones.
    #[error(
        "unsupported proxy URL scheme \"{0}\" (supported: socks5, socks5h, socks4, socks4a, http)"
    )]
    UnsupportedScheme(String),
    /// `https://` proxies (TLS to the proxy) are not supported.
    #[error(
        "unsupported proxy URL scheme \"https\": TLS connections to a proxy are not supported, use an \"http\" proxy"
    )]
    HttpsProxy,
    /// The URL has no host.
    #[error("the proxy URL has no host")]
    MissingHost,
    /// The host is neither an IP address nor a host name made of ASCII letters, digits, `-`,
    /// `.` and `_` (international names are converted to that form).
    #[error(
        "the proxy host must be an IP address or a host name made of letters, digits, '-', '.' and '_'"
    )]
    InvalidHost,
    /// The URL has no port.
    #[error("the proxy URL has no port")]
    MissingPort,
    /// The user name or the password is not valid percent-encoded UTF-8.
    #[error("the proxy user name or password is not valid percent-encoded UTF-8")]
    InvalidCredentials,
    /// A user name must go with a password.
    #[error("a {0} proxy needs a user name to go with a password")]
    IncompleteCredentials(ProxyKind),
    /// Basic authentication can't carry a `:` in the user name.
    #[error("the user name of an http proxy can't contain ':'")]
    ColonInHttpUserName,
    /// SOCKS4 knows only a user id, not a password.
    #[error("SOCKS4 has no passwords: give the user id alone, as {0}://userid@host:port")]
    Socks4Password(ProxyKind),
    /// A SOCKS user name, password or user id is longer than the protocol allows.
    #[error("the proxy user name and password must each be at most 255 bytes long")]
    CredentialsTooLong,
}

/// A parsed proxy URL. See the [module documentation](self) for the supported schemes.
///
/// The `Debug` and `Display` implementations hide the password.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxyConfig {
    kind: ProxyKind,
    host: String,
    port: u16,
    username: Option<String>,
    password: Option<String>,
}

impl ProxyConfig {
    /// Parses a proxy URL such as `socks5h://user:pass@host:1080` or `http://[::1]:3128`.
    ///
    /// The host and the port are required. The host must be an IP address or an ASCII host
    /// name (letters, digits, `-`, `.`, `_`); international names are converted to their ASCII
    /// (`xn--`) form. A user name and a password are percent-decoded, so reserved characters
    /// can be given as `%40` (`@`), `%3A` (`:`), and so on. A SOCKS5 user name without a
    /// password is ignored, with a warning.
    pub fn parse(input: &str) -> Result<Self, ProxyUrlError> {
        let url = url::Url::parse(input).map_err(|e| match e {
            url::ParseError::EmptyHost => ProxyUrlError::MissingHost,
            e => ProxyUrlError::Invalid(e),
        })?;
        let kind = match url.scheme() {
            "socks5" => ProxyKind::Socks5,
            "socks5h" => ProxyKind::Socks5h,
            "socks4" => ProxyKind::Socks4,
            "socks4a" => ProxyKind::Socks4a,
            "http" => ProxyKind::Http,
            "https" => return Err(ProxyUrlError::HttpsProxy),
            other => return Err(ProxyUrlError::UnsupportedScheme(other.to_owned())),
        };

        let host = match url.host() {
            Some(url::Host::Domain("")) | None => return Err(ProxyUrlError::MissingHost),
            // The host of a non-special scheme (`socks5://`, ...) is kept opaque and
            // percent-encoded by the URL parser: decode it and parse it as a domain (IDNA to
            // ASCII), then keep only what every HTTP and SOCKS stack takes as a plain name.
            Some(url::Host::Domain(d)) => {
                let decoded = percent_decode(d).ok_or(ProxyUrlError::InvalidHost)?;
                match url::Host::parse(&decoded).map_err(|_| ProxyUrlError::InvalidHost)? {
                    url::Host::Domain(name) if is_plain_host_name(&name) => name,
                    url::Host::Domain(_) => return Err(ProxyUrlError::InvalidHost),
                    url::Host::Ipv4(ip) => ip.to_string(),
                    url::Host::Ipv6(ip) => ip.to_string(),
                }
            }
            Some(url::Host::Ipv4(ip)) => ip.to_string(),
            Some(url::Host::Ipv6(ip)) => ip.to_string(),
        };

        // `http` is a special scheme for the URL parser, which drops an explicit default port
        // (":80"), so a missing port has to be told from the default one by looking at the text.
        let port = match url.port() {
            Some(p) => p,
            None if authority_has_port(input) => url
                .port_or_known_default()
                .ok_or(ProxyUrlError::MissingPort)?,
            None => return Err(ProxyUrlError::MissingPort),
        };

        let username = match url.username() {
            "" => None,
            u => Some(percent_decode(u).ok_or(ProxyUrlError::InvalidCredentials)?),
        };
        let password = match url.password() {
            None => None,
            Some(p) => Some(percent_decode(p).ok_or(ProxyUrlError::InvalidCredentials)?),
        };

        let (username, password) = match kind {
            ProxyKind::Socks5 | ProxyKind::Socks5h => match (username, password) {
                (None, None) => (None, None),
                (Some(u), Some(p)) if !p.is_empty() => {
                    // RFC 1929: both are 1 to 255 bytes long.
                    if u.len() > 255 || p.len() > 255 {
                        return Err(ProxyUrlError::CredentialsTooLong);
                    }
                    (Some(u), Some(p))
                }
                // Without a password there is nothing to authenticate with: the user name has
                // always been ignored.
                (Some(_), _) => {
                    tracing::warn!(
                        "the user name of a {kind} proxy URL without a password is ignored"
                    );
                    (None, None)
                }
                (None, Some(_)) => return Err(ProxyUrlError::IncompleteCredentials(kind)),
            },
            ProxyKind::Socks4 | ProxyKind::Socks4a => {
                if password.is_some() {
                    return Err(ProxyUrlError::Socks4Password(kind));
                }
                if username.as_ref().is_some_and(|u| u.len() > 255) {
                    return Err(ProxyUrlError::CredentialsTooLong);
                }
                (username, None)
            }
            // Basic authentication allows an empty password, but not a ':' in the user name.
            ProxyKind::Http => match (username, password) {
                (None, None) => (None, None),
                (Some(u), _) if u.contains(':') => {
                    return Err(ProxyUrlError::ColonInHttpUserName);
                }
                (Some(u), p) => (Some(u), Some(p.unwrap_or_default())),
                (None, Some(_)) => return Err(ProxyUrlError::IncompleteCredentials(kind)),
            },
        };

        Ok(Self {
            kind,
            host,
            port,
            username,
            password,
        })
    }

    /// The kind of proxy.
    pub fn kind(&self) -> ProxyKind {
        self.kind
    }

    /// The host of the proxy: a domain name, or an IP address (IPv6 without brackets).
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port of the proxy.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The decoded user name (the user id for SOCKS4), if any. It is a credential: don't log it.
    pub fn username(&self) -> Option<&str> {
        self.username.as_deref()
    }

    /// The decoded password, if any. SOCKS5 proxies have one exactly when they have a user name;
    /// HTTP proxies with a user name have one, possibly empty; SOCKS4 proxies never have one.
    pub fn password(&self) -> Option<&str> {
        self.password.as_deref()
    }

    /// The proxy URL without the user name and the password, e.g. `socks5h://[::1]:1080`.
    pub fn url_without_credentials(&self) -> String {
        format!("{}://{}", self.kind, HostPort(&self.host, self.port))
    }
}

impl FromStr for ProxyConfig {
    type Err = ProxyUrlError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl fmt::Debug for ProxyConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyConfig")
            .field("kind", &self.kind)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username.as_ref().map(|_| "<hidden>"))
            .field("password", &self.password.as_ref().map(|_| "<hidden>"))
            .finish()
    }
}

/// `scheme://[***@]host:port`: the user name is often an account token (and the whole
/// credential for SOCKS4), so it is hidden like the password.
impl fmt::Display for ProxyConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://", self.kind)?;
        if self.username.is_some() {
            f.write_str("***@")?;
        }
        write!(f, "{}", HostPort(&self.host, self.port))
    }
}

/// `host:port`, with brackets around an IPv6 address.
pub(crate) struct HostPort<'a>(pub &'a str, pub u16);

impl fmt::Display for HostPort<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.contains(':') {
            write!(f, "[{}]:{}", self.0, self.1)
        } else {
            write!(f, "{}:{}", self.0, self.1)
        }
    }
}

/// Where a proxy should connect or send to: an IP address, or a host name that the proxy
/// resolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetAddr<'a> {
    /// An IP address and a port.
    Ip(SocketAddr),
    /// A host name and a port.
    Domain(&'a str, u16),
}

impl fmt::Display for TargetAddr<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TargetAddr::Ip(addr) => addr.fmt(f),
            TargetAddr::Domain(host, port) => HostPort(host, *port).fmt(f),
        }
    }
}

impl From<SocketAddr> for TargetAddr<'_> {
    fn from(addr: SocketAddr) -> Self {
        TargetAddr::Ip(addr)
    }
}

/// Whether the authority of a URL that already parsed ends with `:port`.
fn authority_has_port(input: &str) -> bool {
    let rest = input.split_once("://").map_or(input, |(_, r)| r);
    let end = rest.find(['/', '?', '#', '\\']).unwrap_or(rest.len());
    let authority = rest.get(..end).unwrap_or_default();
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let after_host = match host_port.rfind(']') {
        Some(i) => host_port.get(i + 1..).unwrap_or_default(),
        None => host_port,
    };
    after_host
        .rsplit_once(':')
        .is_some_and(|(_, port)| !port.is_empty())
}

/// An ASCII host name every HTTP and SOCKS stack accepts as it is.
fn is_plain_host_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
}

/// Decodes `%XX` escapes. `None` for a broken escape or bytes that aren't UTF-8.
fn percent_decode(s: &str) -> Option<String> {
    fn hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hi = hex(bytes.next()?)?;
            let lo = hex(bytes.next()?)?;
            out.push(hi << 4 | lo);
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> ProxyConfig {
        ProxyConfig::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn every_scheme() {
        for (url, kind) in [
            ("socks5://h:1", ProxyKind::Socks5),
            ("socks5h://h:1", ProxyKind::Socks5h),
            ("socks4://h:1", ProxyKind::Socks4),
            ("socks4a://h:1", ProxyKind::Socks4a),
            ("http://h:1", ProxyKind::Http),
            ("SOCKS5H://h:1", ProxyKind::Socks5h),
        ] {
            let c = parse(url);
            assert_eq!(c.kind(), kind, "{url}");
            assert_eq!((c.host(), c.port()), ("h", 1), "{url}");
            assert_eq!((c.username(), c.password()), (None, None), "{url}");
        }
    }

    #[test]
    fn kinds() {
        assert!(ProxyKind::Socks5.can_relay_udp());
        assert!(ProxyKind::Socks5h.can_relay_udp());
        assert!(!ProxyKind::Socks4.can_relay_udp());
        assert!(!ProxyKind::Socks4a.can_relay_udp());
        assert!(!ProxyKind::Http.can_relay_udp());
        assert!(!ProxyKind::Socks5.resolves_hostnames());
        assert!(ProxyKind::Socks5h.resolves_hostnames());
        assert!(!ProxyKind::Socks4.resolves_hostnames());
        assert!(ProxyKind::Socks4a.resolves_hostnames());
        assert!(ProxyKind::Http.resolves_hostnames());
    }

    #[test]
    fn credentials_are_percent_decoded() {
        let c = parse("socks5h://us%3Aer:p%40ss%2Fw%25rd%20%C3%A9@proxy.example:1080");
        assert_eq!(c.username(), Some("us:er"));
        assert_eq!(c.password(), Some("p@ss/w%rd é"));
        assert_eq!(c.host(), "proxy.example");

        let c = parse("http://user:@h:3128");
        assert_eq!((c.username(), c.password()), (Some("user"), Some("")));
        let c = parse("http://user@h:3128");
        assert_eq!((c.username(), c.password()), (Some("user"), Some("")));

        let c = parse("socks4a://some%20id@h:1080");
        assert_eq!((c.username(), c.password()), (Some("some id"), None));
    }

    #[test]
    fn ip_literals() {
        let c = parse("socks5://[::1]:1080");
        assert_eq!(c.host(), "::1");
        assert_eq!(c.url_without_credentials(), "socks5://[::1]:1080");
        let c = parse("http://u:p@[2001:db8::1]:8080");
        assert_eq!(c.host(), "2001:db8::1");
        assert_eq!(c.port(), 8080);
        assert_eq!(c.to_string(), "http://***@[2001:db8::1]:8080");
        let c = parse("socks4://10.0.0.1:1080");
        assert_eq!(c.host(), "10.0.0.1");
    }

    #[test]
    fn host_shapes() {
        for scheme in ["socks5", "socks5h", "socks4", "socks4a", "http"] {
            for (host, want) in [
                ("127.0.0.1", "127.0.0.1"),
                ("[::1]", "::1"),
                ("Proxy-1.example_x", "proxy-1.example_x"),
                ("прокси.example", "xn--h1adldfi.example"),
                (
                    "%D0%BF%D1%80%D0%BE%D0%BA%D1%81%D0%B8.example",
                    "xn--h1adldfi.example",
                ),
            ] {
                let url = format!("{scheme}://{host}:1080");
                let c = parse(&url);
                assert_eq!(c.host(), want, "{url}");
                // What the HTTP stack parses the proxy from must be a valid URI.
                let uri: http::Uri = c.url_without_credentials().parse().expect(&url);
                assert_eq!(
                    uri.host().map(|h| h.trim_matches(['[', ']'])),
                    Some(want),
                    "{url}"
                );
            }
            for host in [
                "a%25b", "a%22b", "a%60b", "a%7Bb", "a%7Db", "a%20b", "a%00b", "a%0Ab", "a\"b",
                "a{b", "a}b", "a`b", "a%ffb", "a!b", "a$b",
            ] {
                let url = format!("{scheme}://{host}:1080");
                assert!(
                    matches!(
                        ProxyConfig::parse(&url),
                        Err(ProxyUrlError::InvalidHost | ProxyUrlError::Invalid(_))
                    ),
                    "{url} was accepted"
                );
            }
        }
    }

    #[test]
    fn lone_socks5_user_name_is_ignored() {
        for url in ["socks5://u@h:1", "socks5h://u:@h:1"] {
            let c = parse(url);
            assert_eq!((c.username(), c.password()), (None, None), "{url}");
        }
    }

    #[test]
    fn explicit_default_port() {
        assert_eq!(parse("http://h:80").port(), 80);
        assert_eq!(parse("http://u:p@[::1]:80/").port(), 80);
        assert_eq!(
            ProxyConfig::parse("http://h"),
            Err(ProxyUrlError::MissingPort)
        );
        assert_eq!(
            ProxyConfig::parse("http://h:/"),
            Err(ProxyUrlError::MissingPort)
        );
        assert_eq!(
            ProxyConfig::parse("http://[::1]/"),
            Err(ProxyUrlError::MissingPort)
        );
    }

    #[test]
    fn errors() {
        use ProxyUrlError::*;
        for (url, err) in [
            ("ftp://h:1", UnsupportedScheme("ftp".into())),
            ("socks6://h:1", UnsupportedScheme("socks6".into())),
            ("https://h:1", HttpsProxy),
            ("socks5://h", MissingPort),
            ("socks5://:1", MissingHost),
            (
                "socks5h://:p@h:1",
                IncompleteCredentials(ProxyKind::Socks5h),
            ),
            ("http://us%3Aer:p@h:1", ColonInHttpUserName),
            ("socks5://:p@h:1", IncompleteCredentials(ProxyKind::Socks5)),
            ("http://:p@h:1", IncompleteCredentials(ProxyKind::Http)),
            ("socks4://u:p@h:1", Socks4Password(ProxyKind::Socks4)),
            ("socks5://u:%ff@h:1", InvalidCredentials),
            ("socks5://u:%4@h:1", InvalidCredentials),
        ] {
            assert_eq!(ProxyConfig::parse(url), Err(err), "{url}");
        }
        let long = "a".repeat(256);
        assert_eq!(
            ProxyConfig::parse(&format!("socks5://u:{long}@h:1")),
            Err(CredentialsTooLong)
        );
        assert_eq!(
            ProxyConfig::parse(&format!("socks4://{long}@h:1")),
            Err(CredentialsTooLong)
        );
        assert!(matches!(ProxyConfig::parse("not a url"), Err(Invalid(_))));
        assert!(ProxyConfig::parse("socks5://").is_err());
        assert!(matches!(
            ProxyConfig::parse("socks5://h:99999"),
            Err(Invalid(_))
        ));
    }

    #[test]
    fn credentials_never_show() {
        let secret = "hunter2-SECRET";
        let user = "account-TOKEN";
        let ok = parse(&format!("socks5h://{user}:{secret}@h:1"));
        let id = parse(&format!("socks4a://{user}@h:1"));
        for s in [
            format!("{ok}"),
            format!("{ok:?}"),
            format!("{ok:#?}"),
            format!("{id}"),
            format!("{id:?}"),
        ] {
            assert!(!s.contains(secret) && !s.contains(user), "{s}");
        }
        assert_eq!(ok.to_string(), "socks5h://***@h:1");
        assert_eq!(id.to_string(), "socks4a://***@h:1");
        assert_eq!(ok.url_without_credentials(), "socks5h://h:1");

        for url in [
            format!("https://user:{secret}@h:1"),
            format!("gopher://user:{secret}@h:1"),
            format!("socks5://user:{secret}@h"),
            format!("socks5://user:{secret}@:1"),
            format!("socks5://user:{secret}@h:999999"),
            format!("socks5://user:{secret}%ff@h:1"),
            format!("socks4://user:{secret}@h:1"),
            format!("socks5://user:{secret}{}@h:1", "x".repeat(300)),
            format!("socks5 ://user:{secret}@h:1"),
        ] {
            let e = ProxyConfig::parse(&url).unwrap_err();
            for s in [format!("{e}"), format!("{e:?}")] {
                assert!(!s.contains(secret), "{url}: {s}");
            }
        }
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%2Fb%2fc").as_deref(), Some("a/b/c"));
        assert_eq!(percent_decode("%"), None);
        assert_eq!(percent_decode("%g0"), None);
        assert_eq!(percent_decode("%c3"), None);
        assert_eq!(percent_decode("").as_deref(), Some(""));
    }
}
