use std::{
    marker::PhantomData,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};

use serde::{Deserialize, Deserializer, Serialize};

enum IpOctets {
    V4([u8; 4]),
    V6([u8; 16]),
}

impl IpOctets {
    fn as_slice(&self) -> &[u8] {
        match &self {
            IpOctets::V4(s) => s,
            IpOctets::V6(s) => s,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PeerIP<T>(pub T);
pub type PeerIP4 = PeerIP<Ipv4Addr>;
pub type PeerIP6 = PeerIP<Ipv6Addr>;
pub type PeerIPAny = PeerIP<IpAddr>;

trait IpLike: Sized {
    fn octets(&self) -> IpOctets;
    fn try_from_slice(b: &[u8]) -> Option<Self>;
    fn expecting() -> &'static str;
}

impl IpLike for Ipv4Addr {
    fn octets(&self) -> IpOctets {
        IpOctets::V4(self.octets())
    }

    fn try_from_slice(b: &[u8]) -> Option<Self> {
        let arr: [u8; 4] = b.try_into().ok()?;
        Some(arr.into())
    }

    fn expecting() -> &'static str {
        "expecting 4 bytes of ipv4"
    }
}

impl IpLike for Ipv6Addr {
    fn octets(&self) -> IpOctets {
        IpOctets::V6(self.octets())
    }

    fn try_from_slice(b: &[u8]) -> Option<Self> {
        let arr: [u8; 16] = b.try_into().ok()?;
        Some(arr.into())
    }

    fn expecting() -> &'static str {
        "expecting 16 bytes of ipv6"
    }
}

impl IpLike for IpAddr {
    fn octets(&self) -> IpOctets {
        match self {
            IpAddr::V4(ipv4_addr) => IpOctets::V4(ipv4_addr.octets()),
            IpAddr::V6(ipv6_addr) => IpOctets::V6(ipv6_addr.octets()),
        }
    }

    fn try_from_slice(b: &[u8]) -> Option<Self> {
        match b.len() {
            4 => Ipv4Addr::try_from_slice(b).map(Into::into),
            16 => Ipv6Addr::try_from_slice(b).map(Into::into),
            _ => None,
        }
    }

    fn expecting() -> &'static str {
        "expecting 4 or 16 bytes of ipv4 or ipv6"
    }
}

impl<T> Serialize for PeerIP<T>
where
    T: IpLike,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bytes(self.0.octets().as_slice())
    }
}

impl<'de, T> Deserialize<'de> for PeerIP<T>
where
    T: IpLike,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor<T> {
            p: PhantomData<T>,
        }
        impl<'de, T> serde::de::Visitor<'de> for Visitor<T>
        where
            T: IpLike,
        {
            type Value = PeerIP<T>;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(T::expecting())
            }

            fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                T::try_from_slice(v)
                    .map(PeerIP)
                    .ok_or_else(|| E::custom(T::expecting()))
            }
        }
        deserializer.deserialize_bytes(Visitor {
            p: Default::default(),
        })
    }
}
