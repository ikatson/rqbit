//! I2P names and destinations, and the synthetic socket addresses I2P peers get.
//!
//! An I2P peer is not reachable at an IP address: it is identified by its *destination*, a
//! public key structure of at least 387 bytes. Its *hash* is the SHA-256 of the binary
//! destination, and its *b32 name* is that hash in lowercase base32 followed by `.b32.i2p`.
//! Text forms of destinations use the I2P base64 alphabet (standard base64 with `-` and `~`
//! instead of `+` and `/`).
//!
//! The peer machinery of a BitTorrent client is keyed by [`SocketAddr`]. I2P peers get a
//! synthetic one inside the IPv6 discard prefix `0100::/64` (RFC 6666): no real peer can ever
//! have such an address, so the two never collide. Use [`is_synthetic`] to recognise them.

use std::net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV6};

use anyhow::{Context, bail};
use bytes::Bytes;
use data_encoding::{BASE32_NOPAD, BASE64};
#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
use sha1w::ISha256;

/// The first 64 bits of every synthetic address: the IPv6 discard prefix `0100::/64`.
const SYNTHETIC_PREFIX: u64 = 0x0100_0000_0000_0000;

/// The port of every synthetic address. I2P has no ports; any non-zero value works.
pub const SYNTHETIC_PORT: u16 = 6881;

/// The smallest binary destination: a 256-byte encryption key, a 128-byte signing key and a
/// 3-byte certificate header.
const MIN_DESTINATION_LEN: usize = 387;

/// Destinations are never bigger than this (the certificate length is a 16-bit value, but the
/// key certificates in use carry a few bytes only).
const MAX_DESTINATION_LEN: usize = 1024;

/// The suffix of the b32 names.
pub const B32_SUFFIX: &str = ".b32.i2p";

/// Whether `addr` is a synthetic address of an I2P peer.
pub fn is_synthetic(addr: &SocketAddr) -> bool {
    is_synthetic_ip(&addr.ip())
}

/// Whether `ip` is inside the range of the synthetic addresses of I2P peers.
pub fn is_synthetic_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(_) => false,
        IpAddr::V6(v6) => (v6.to_bits() >> 64) as u64 == SYNTHETIC_PREFIX,
    }
}

/// The synthetic address number `index`.
pub fn synthetic_addr(index: u64) -> SocketAddr {
    let bits = (u128::from(SYNTHETIC_PREFIX) << 64) | u128::from(index);
    SocketAddr::V6(SocketAddrV6::new(
        Ipv6Addr::from_bits(bits),
        SYNTHETIC_PORT,
        0,
        0,
    ))
}

/// The number of a synthetic address, None for any other address.
pub fn synthetic_index(addr: &SocketAddr) -> Option<u64> {
    match addr {
        SocketAddr::V6(v6) if is_synthetic(addr) => Some(v6.ip().to_bits() as u64),
        _ => None,
    }
}

/// Encodes bytes with the I2P base64 alphabet (with padding).
pub fn i2p_base64_encode(bytes: &[u8]) -> String {
    BASE64
        .encode(bytes)
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '~',
            c => c,
        })
        .collect()
}

/// Decodes I2P base64 (with padding). Standard base64 (`+`, `/`) is refused.
pub fn i2p_base64_decode(text: &str) -> anyhow::Result<Vec<u8>> {
    let standard: String = text
        .chars()
        .map(|c| match c {
            '-' => Ok('+'),
            '~' => Ok('/'),
            '+' | '/' => Err(c),
            c => Ok(c),
        })
        .collect::<Result<_, _>>()
        .map_err(|c| anyhow::anyhow!("{c:?} is not in the I2P base64 alphabet"))?;
    BASE64
        .decode(standard.as_bytes())
        .context("invalid I2P base64")
}

/// The SHA-256 of a binary destination: what identifies an I2P peer.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct I2pHash(pub [u8; 32]);

impl I2pHash {
    /// The `….b32.i2p` name.
    pub fn b32_name(&self) -> String {
        let mut name = BASE32_NOPAD.encode(&self.0).to_ascii_lowercase();
        name.push_str(B32_SUFFIX);
        name
    }

    /// Parses a `….b32.i2p` name (case-insensitive). None for any other name, including the
    /// longer "encrypted lease set" b33 names.
    pub fn from_b32_name(name: &str) -> Option<Self> {
        let lower = name.to_ascii_lowercase();
        let encoded = lower.strip_suffix(B32_SUFFIX)?;
        if encoded.len() != 52 {
            return None;
        }
        let bytes = BASE32_NOPAD
            .decode(encoded.to_ascii_uppercase().as_bytes())
            .ok()?;
        Some(Self(bytes.try_into().ok()?))
    }
}

impl std::fmt::Debug for I2pHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.b32_name())
    }
}

impl std::fmt::Display for I2pHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.b32_name())
    }
}

/// A binary I2P destination whose structure (keys and certificate length) was checked.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct I2pDestination(Bytes);

impl I2pDestination {
    /// Checks the structure of a binary destination.
    pub fn from_bytes(bytes: Bytes) -> anyhow::Result<Self> {
        if bytes.len() < MIN_DESTINATION_LEN {
            bail!(
                "a destination is at least {MIN_DESTINATION_LEN} bytes, got {}",
                bytes.len()
            );
        }
        if bytes.len() > MAX_DESTINATION_LEN {
            bail!("a destination of {} bytes is too long", bytes.len());
        }
        let cert_len = usize::from(u16::from_be_bytes([bytes[385], bytes[386]]));
        if bytes.len() != MIN_DESTINATION_LEN + cert_len {
            bail!(
                "a destination with a {cert_len}-byte certificate is {} bytes, got {}",
                MIN_DESTINATION_LEN + cert_len,
                bytes.len()
            );
        }
        Ok(Self(bytes))
    }

    /// Parses the I2P base64 text form.
    pub fn from_i2p_base64(text: &str) -> anyhow::Result<Self> {
        if text.len() > MAX_DESTINATION_LEN * 4 / 3 + 4 {
            bail!("a destination of {} characters is too long", text.len());
        }
        Self::from_bytes(i2p_base64_decode(text)?.into())
    }

    /// The I2P base64 text form.
    pub fn to_i2p_base64(&self) -> String {
        i2p_base64_encode(&self.0)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// The SHA-256 of the binary destination. Needs one of the `sha1-*` features (which also
    /// provide SHA-256).
    #[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
    pub fn hash(&self) -> I2pHash {
        let mut h = sha1w::Sha256::new();
        h.update(&self.0);
        I2pHash(h.finish())
    }
}

impl std::fmt::Debug for I2pDestination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        #[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
        return write!(f, "I2pDestination({})", self.hash());
        #[cfg(not(any(feature = "sha1-ring", feature = "sha1-crypto-hash")))]
        return write!(f, "I2pDestination({} bytes)", self.0.len());
    }
}

/// Whether `host` is a name inside the I2P network (`….i2p`, case-insensitive).
pub fn is_i2p_host(host: &str) -> bool {
    host.len() > 4
        && host
            .get(host.len() - 4..)
            .is_some_and(|s| s.eq_ignore_ascii_case(".i2p"))
}

#[cfg(all(test, any(feature = "sha1-ring", feature = "sha1-crypto-hash")))]
mod tests {
    use super::*;

    /// A destination with a key certificate (type 5, 4 bytes: signature type 7, crypto type
    /// 0), the 384 key bytes being `(i * 7 + 3) & 0xff`. The expected text forms were computed
    /// independently with Python:
    ///
    /// ```text
    /// d = bytes((i*7+3)&0xff for i in range(384)) + bytes([5,0,4,0,7,0,0])
    /// base64.b64encode(d).decode().replace('+','-').replace('/','~')
    /// base64.b32encode(hashlib.sha256(d).digest()).decode().lower().rstrip('=') + '.b32.i2p'
    /// ```
    fn vector() -> Vec<u8> {
        let mut d: Vec<u8> = (0..384u32).map(|i| ((i * 7 + 3) & 0xff) as u8).collect();
        d.extend_from_slice(&[5, 0, 4, 0, 7, 0, 0]);
        d
    }

    const VECTOR_BASE64: &str = "AwoRGB8mLTQ7QklQV15lbHN6gYiPlp2kq7K5wMfO1dzj6vH4~wYNFBsiKTA3PkVMU1phaG92fYSLkpmgp661vMPK0djf5u30-wIJEBceJSwzOkFIT1ZdZGtyeYCHjpWco6qxuL~GzdTb4unw9~4FDBMaISgvNj1ES1JZYGdudXyDipGYn6attLvCydDX3uXs8~oBCA8WHSQrMjlAR05VXGNqcXh~ho2Um6KpsLe-xczT2uHo7~b9BAsSGSAnLjU8Q0pRWF9mbXR7gomQl56lrLO6wcjP1t3k6~L5AAcOFRwjKjE4P0ZNVFtiaXB3foWMk5qhqK-2vcTL0tng5-71~AMKERgfJi00O0JJUFdeZWxzeoGIj5adpKuyucDHztXc4-rx-P8GDRQbIikwNz5FTFNaYWhvdn2Ei5KZoKeutbzDytHY3-bt9PsCCRAXHiUsMzpBSE9WXWRrcnmAh46VnKOqsbi~xs3U2-Lp8Pf-BQwTGiEoLzY9REtSWWBnbnV8BQAEAAcAAA==";
    const VECTOR_B32: &str = "caepgktmelgd47vxo3xhcfqr4wdvyd77hfcg4ng2snopukspybta.b32.i2p";

    #[test]
    fn destination_to_base64_and_b32() {
        let d = I2pDestination::from_bytes(vector().into()).unwrap();
        assert_eq!(d.to_i2p_base64(), VECTOR_BASE64);
        assert_eq!(d.hash().b32_name(), VECTOR_B32);
        let parsed = I2pDestination::from_i2p_base64(VECTOR_BASE64).unwrap();
        assert_eq!(parsed, d);
        assert_eq!(I2pHash::from_b32_name(VECTOR_B32), Some(d.hash()));
        assert_eq!(
            I2pHash::from_b32_name(&VECTOR_B32.to_ascii_uppercase()),
            Some(d.hash())
        );
    }

    #[test]
    fn malformed_destinations_are_refused() {
        // Standard base64 characters are not I2P base64.
        let standard = VECTOR_BASE64.replace('-', "+");
        assert!(I2pDestination::from_i2p_base64(&standard).is_err());
        // Too short.
        assert!(I2pDestination::from_bytes(vec![0u8; 386].into()).is_err());
        // The certificate length disagrees with the length.
        let mut d = vector();
        d.push(0);
        assert!(I2pDestination::from_bytes(d.into()).is_err());
        let mut d = vector();
        d[386] = 5;
        assert!(I2pDestination::from_bytes(d.into()).is_err());
        // Garbage.
        assert!(I2pDestination::from_i2p_base64("not a destination").is_err());
        assert!(I2pDestination::from_i2p_base64(&"A".repeat(100_000)).is_err());
    }

    #[test]
    fn b32_names() {
        assert_eq!(I2pHash::from_b32_name("tracker.i2p"), None);
        // 51 characters.
        assert_eq!(I2pHash::from_b32_name(&VECTOR_B32[1..]), None);
        // Not base32.
        let bad = VECTOR_B32.replacen('c', "1", 1);
        assert_eq!(I2pHash::from_b32_name(&bad), None);
        // Non-zero trailing bits (the last character carries 1 bit of the hash and 4 zeros).
        let bad = VECTOR_B32.replacen("bta.", "btb.", 1);
        assert_eq!(I2pHash::from_b32_name(&bad), None);
    }

    #[test]
    fn synthetic_addresses() {
        let a = synthetic_addr(5);
        assert_eq!(a.to_string(), "[100::5]:6881");
        assert!(is_synthetic(&a));
        assert_eq!(synthetic_index(&a), Some(5));
        assert_eq!(synthetic_index(&synthetic_addr(u64::MAX)), Some(u64::MAX));
        for real in [
            "1.0.0.5:6881",
            "[::1]:6881",
            "[100:0:0:1::5]:6881",
            "[2001:db8::1]:6881",
            "[::ffff:1.0.0.5]:6881",
        ] {
            let real: SocketAddr = real.parse().unwrap();
            assert!(!is_synthetic(&real), "{real}");
            assert_eq!(synthetic_index(&real), None);
        }
    }

    #[test]
    fn i2p_hosts() {
        assert!(is_i2p_host("tracker.i2p"));
        assert!(is_i2p_host("TRACKER.I2P"));
        assert!(is_i2p_host(VECTOR_B32));
        assert!(!is_i2p_host(".i2p"));
        assert!(!is_i2p_host("tracker.i2p.example.org"));
        assert!(!is_i2p_host("i2p"));
        assert!(!is_i2p_host("ab"));
    }
}
