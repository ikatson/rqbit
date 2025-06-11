use std::{
    marker::PhantomData,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
};

use serde::{Deserialize, Serialize};

mod small_slice {
    pub struct SmallSlice {
        // enough to hold IPv6 + port
        data: [u8; 18],
        len: usize,
    }

    impl SmallSlice {
        #[allow(clippy::new_without_default)]
        pub fn new() -> Self {
            Self {
                data: [0u8; 18],
                len: 0,
            }
        }

        pub fn new_from_buf(buf: &[u8]) -> Self {
            let mut s = Self::new();
            s.extend(buf);
            s
        }

        pub fn extend(&mut self, buf: &[u8]) {
            self.data[self.len..self.len + buf.len()].copy_from_slice(buf);
        }

        pub fn as_slice(&self) -> &[u8] {
            &self.data[..self.len]
        }
    }
}

use small_slice::SmallSlice;

#[derive(Clone, Copy)]
pub struct Compact<T>(pub T);

impl<T> From<T> for Compact<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

pub struct CompactList<T>(Vec<T>);

impl<T: core::fmt::Debug> core::fmt::Debug for Compact<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

pub type CompactIpV4 = Compact<Ipv4Addr>;
pub type CompactIpV6 = Compact<Ipv6Addr>;
pub type CompactIpAddr = Compact<IpAddr>;
pub type CompactSocketAddr4 = Compact<SocketAddrV4>;
pub type CompactSocketAddr6 = Compact<SocketAddrV6>;
pub type CompactSocketAddr = Compact<SocketAddr>;

pub trait CompactSerialize: Sized {
    fn fixed_len() -> Option<usize>;
    fn expecting() -> &'static str;
    fn as_slice(&self) -> SmallSlice;
    fn from_slice_unchecked(buf: &[u8]) -> Self {
        Self::from_slice(buf).unwrap()
    }
    fn from_slice(buf: &[u8]) -> Option<Self>;
}

impl CompactSerialize for Ipv4Addr {
    fn as_slice(&self) -> SmallSlice {
        SmallSlice::new_from_buf(&self.octets())
    }

    fn from_slice(buf: &[u8]) -> Option<Self> {
        Some(Self::from(
            TryInto::<[u8; 4]>::try_into(buf.get(..4)?).unwrap(),
        ))
    }

    fn expecting() -> &'static str {
        "4 bytes for IPv4"
    }

    fn fixed_len() -> Option<usize> {
        Some(4)
    }
}

impl CompactSerialize for Ipv6Addr {
    fn as_slice(&self) -> SmallSlice {
        SmallSlice::new_from_buf(&self.octets())
    }

    fn from_slice(buf: &[u8]) -> Option<Self> {
        Some(Self::from(
            TryInto::<[u8; 16]>::try_into(buf.get(..16)?).unwrap(),
        ))
    }

    fn expecting() -> &'static str {
        "16 bytes for IPv6"
    }

    fn fixed_len() -> Option<usize> {
        Some(16)
    }
}

impl CompactSerialize for IpAddr {
    fn as_slice(&self) -> SmallSlice {
        match self {
            IpAddr::V4(a) => a.as_slice(),
            IpAddr::V6(a) => a.as_slice(),
        }
    }

    fn from_slice(buf: &[u8]) -> Option<Self> {
        match buf.len() {
            4 => Some(Ipv4Addr::from_slice_unchecked(buf).into()),
            16 => Some(Ipv6Addr::from_slice_unchecked(buf).into()),
            _ => None,
        }
    }

    fn expecting() -> &'static str {
        "16 bytes for IPv6 or 4 bytes for IPv4"
    }

    fn fixed_len() -> Option<usize> {
        None
    }
}

impl CompactSerialize for SocketAddrV4 {
    fn as_slice(&self) -> SmallSlice {
        let mut s = self.ip().as_slice();
        s.extend(&self.port().to_be_bytes());
        s
    }

    fn from_slice(buf: &[u8]) -> Option<Self> {
        if buf.len() != 6 {
            return None;
        }
        let ip = Ipv4Addr::from_slice_unchecked(&buf[..4]);
        let port = u16::from_be_bytes([buf[4], buf[5]]);
        Some(SocketAddrV4::new(ip, port))
    }

    fn expecting() -> &'static str {
        "6 bytes for SocketAddrV4"
    }

    fn fixed_len() -> Option<usize> {
        Some(6)
    }
}

impl CompactSerialize for SocketAddrV6 {
    fn as_slice(&self) -> SmallSlice {
        let mut s = self.ip().as_slice();
        s.extend(&self.port().to_be_bytes());
        s
    }

    fn from_slice(buf: &[u8]) -> Option<Self> {
        if buf.len() != 18 {
            return None;
        }
        let ip = Ipv6Addr::from_slice_unchecked(&buf[..16]);
        let port = u16::from_be_bytes([buf[16], buf[17]]);
        Some(SocketAddrV6::new(ip, port, 0, 0))
    }

    fn expecting() -> &'static str {
        "18 bytes for SocketAddrV6"
    }

    fn fixed_len() -> Option<usize> {
        Some(18)
    }
}

impl CompactSerialize for SocketAddr {
    fn as_slice(&self) -> SmallSlice {
        let mut s = self.ip().as_slice();
        s.extend(&self.port().to_be_bytes());
        s
    }

    fn from_slice(buf: &[u8]) -> Option<Self> {
        match buf.len() {
            6 => Some(SocketAddrV4::from_slice_unchecked(buf).into()),
            18 => Some(SocketAddrV6::from_slice_unchecked(buf).into()),
            _ => None,
        }
    }

    fn expecting() -> &'static str {
        "18 bytes for SocketAddrV6 or 6 bytes for SocketAddrV4"
    }

    fn fixed_len() -> Option<usize> {
        None
    }
}

impl<T: CompactSerialize> Serialize for Compact<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bytes(self.0.as_slice().as_slice())
    }
}

impl<'de, T: CompactSerialize> Deserialize<'de> for Compact<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor<T> {
            _phantom: PhantomData<T>,
        }
        impl<'de, T: CompactSerialize> serde::de::Visitor<'de> for Visitor<T> {
            type Value = Compact<T>;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(T::expecting())
            }

            fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match T::from_slice(v) {
                    Some(v) => Ok(Compact(v)),
                    None => Err(E::custom(T::expecting())),
                }
            }
        }
        deserializer.deserialize_bytes(Visitor {
            _phantom: Default::default(),
        })
    }
}
