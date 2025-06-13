use std::{
    marker::PhantomData,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
};

use buffers::ByteBufOwned;
use bytes::BytesMut;
use clone_to_owned::CloneToOwned;
use serde::{Deserialize, Serialize};

mod small_slice {
    pub struct SmallSlice<const N: usize> {
        data: [u8; N],
        len: usize,
    }

    impl<const N: usize> AsRef<[u8]> for SmallSlice<N> {
        fn as_ref(&self) -> &[u8] {
            self.as_slice()
        }
    }

    impl<const N: usize> SmallSlice<N> {
        #[allow(clippy::new_without_default)]
        pub fn new() -> Self {
            Self {
                data: [0u8; N],
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
            self.len += buf.len();
        }

        pub fn as_slice(&self) -> &[u8] {
            &self.data[..self.len]
        }
    }
}

pub use small_slice::SmallSlice;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Compact<T>(pub T);

impl<T> From<T> for Compact<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

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
    type Slice: AsRef<[u8]>;

    fn expecting() -> &'static str;
    fn as_slice(&self) -> Self::Slice;
    fn from_slice_unchecked_len(buf: &[u8]) -> Self {
        Self::from_slice(buf).unwrap()
    }
    fn from_slice(buf: &[u8]) -> Option<Self>;
}

pub trait CompactSerializeFixedLen {
    fn fixed_len() -> usize;
}

impl CompactSerialize for Ipv4Addr {
    type Slice = [u8; 4];

    fn as_slice(&self) -> Self::Slice {
        self.octets()
    }

    fn from_slice(buf: &[u8]) -> Option<Self> {
        Some(Self::from(
            TryInto::<[u8; 4]>::try_into(buf.get(..4)?).unwrap(),
        ))
    }

    fn from_slice_unchecked_len(buf: &[u8]) -> Self {
        Self::from(TryInto::<[u8; 4]>::try_into(buf).unwrap())
    }

    fn expecting() -> &'static str {
        "4 bytes for IPv4"
    }
}

impl CompactSerializeFixedLen for Ipv4Addr {
    fn fixed_len() -> usize {
        4
    }
}

impl CompactSerialize for Ipv6Addr {
    type Slice = [u8; 16];

    fn as_slice(&self) -> Self::Slice {
        self.octets()
    }

    fn from_slice(buf: &[u8]) -> Option<Self> {
        Some(Self::from(
            TryInto::<[u8; 16]>::try_into(buf.get(..16)?).unwrap(),
        ))
    }

    fn from_slice_unchecked_len(buf: &[u8]) -> Self {
        Self::from(TryInto::<[u8; 16]>::try_into(buf).unwrap())
    }

    fn expecting() -> &'static str {
        "16 bytes for IPv6"
    }
}

impl CompactSerializeFixedLen for Ipv6Addr {
    fn fixed_len() -> usize {
        16
    }
}

impl CompactSerialize for IpAddr {
    type Slice = SmallSlice<16>;

    fn as_slice(&self) -> Self::Slice {
        match self {
            IpAddr::V4(a) => SmallSlice::new_from_buf(&a.as_slice()),
            IpAddr::V6(a) => SmallSlice::new_from_buf(&a.as_slice()),
        }
    }

    fn from_slice(buf: &[u8]) -> Option<Self> {
        match buf.len() {
            4 => Some(Ipv4Addr::from_slice_unchecked_len(buf).into()),
            16 => Some(Ipv6Addr::from_slice_unchecked_len(buf).into()),
            _ => None,
        }
    }

    fn expecting() -> &'static str {
        "16 bytes for IPv6 or 4 bytes for IPv4"
    }
}

impl CompactSerialize for SocketAddrV4 {
    type Slice = [u8; 6];

    fn as_slice(&self) -> Self::Slice {
        let mut data = [0u8; 6];
        data[..4].copy_from_slice(&self.ip().octets());
        data[4..6].copy_from_slice(&self.port().to_be_bytes());
        data
    }

    fn from_slice(buf: &[u8]) -> Option<Self> {
        if buf.len() != 6 {
            return None;
        }
        Some(Self::from_slice_unchecked_len(buf))
    }

    fn from_slice_unchecked_len(buf: &[u8]) -> Self {
        let ip = Ipv4Addr::from_slice_unchecked_len(&buf[..4]);
        let port = u16::from_be_bytes([buf[4], buf[5]]);
        SocketAddrV4::new(ip, port)
    }

    fn expecting() -> &'static str {
        "6 bytes for SocketAddrV4"
    }
}

impl CompactSerializeFixedLen for SocketAddrV4 {
    fn fixed_len() -> usize {
        6
    }
}

impl CompactSerialize for SocketAddrV6 {
    type Slice = [u8; 18];

    fn as_slice(&self) -> Self::Slice {
        let mut data = [0u8; 18];
        data[..16].copy_from_slice(&self.ip().octets());
        data[16..18].copy_from_slice(&self.port().to_be_bytes());
        data
    }

    fn from_slice(buf: &[u8]) -> Option<Self> {
        if buf.len() != 18 {
            return None;
        }
        Some(Self::from_slice_unchecked_len(buf))
    }

    fn from_slice_unchecked_len(buf: &[u8]) -> Self {
        let ip = Ipv6Addr::from_slice_unchecked_len(&buf[..16]);
        let port = u16::from_be_bytes([buf[16], buf[17]]);
        SocketAddrV6::new(ip, port, 0, 0)
    }

    fn expecting() -> &'static str {
        "18 bytes for SocketAddrV6"
    }
}

impl CompactSerializeFixedLen for SocketAddrV6 {
    fn fixed_len() -> usize {
        18
    }
}

impl CompactSerialize for SocketAddr {
    type Slice = SmallSlice<18>;

    fn as_slice(&self) -> Self::Slice {
        let mut s = SmallSlice::new_from_buf(self.ip().as_slice().as_ref());
        s.extend(&self.port().to_be_bytes());
        s
    }

    fn from_slice(buf: &[u8]) -> Option<Self> {
        match buf.len() {
            6 => Some(SocketAddrV4::from_slice_unchecked_len(buf).into()),
            18 => Some(SocketAddrV6::from_slice_unchecked_len(buf).into()),
            _ => None,
        }
    }

    fn expecting() -> &'static str {
        "18 bytes for SocketAddrV6 or 6 bytes for SocketAddrV4"
    }
}

impl<T: CompactSerialize> Serialize for Compact<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bytes(self.0.as_slice().as_ref())
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

pub struct CompactListInBuffer<Buf, T> {
    buf: Buf,
    _phantom: PhantomData<T>,
}

impl<Buf, T> core::fmt::Debug for CompactListInBuffer<Buf, T>
where
    Buf: AsRef<[u8]>,
    T: core::fmt::Debug + CompactSerialize + CompactSerializeFixedLen,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        struct IterDebug<I>(I);
        impl<I> core::fmt::Debug for IterDebug<I>
        where
            I: Iterator + Clone,
            I::Item: core::fmt::Debug,
        {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.debug_list().entries(self.0.clone()).finish()
            }
        }
        write!(f, "{:?}", IterDebug(self.iter()))
    }
}

pub type CompactListInBufferOwned<T> = CompactListInBuffer<ByteBufOwned, T>;

impl<T> CompactListInBufferOwned<T>
where
    T: CompactSerialize + CompactSerializeFixedLen,
{
    pub fn new_from_iter(it: impl Iterator<Item = T>) -> Self {
        let mut b = BytesMut::new();
        for item in it {
            b.extend_from_slice(item.as_slice().as_ref());
        }
        Self {
            buf: b.freeze().into(),
            _phantom: Default::default(),
        }
    }
}

impl<Buf, T> CompactListInBuffer<Buf, T>
where
    Buf: AsRef<[u8]>,
    T: CompactSerialize + CompactSerializeFixedLen,
{
    pub fn is_empty(&self) -> bool {
        self.buf.as_ref().is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = T> + Clone {
        self.buf
            .as_ref()
            .chunks_exact(T::fixed_len())
            .map(|chunk| T::from_slice_unchecked_len(chunk))
    }

    pub fn get(&self, idx: usize) -> Option<T> {
        let offset = idx * T::fixed_len();
        let end = offset + T::fixed_len();
        let b = self.buf.as_ref().get(offset..end)?;
        Some(T::from_slice_unchecked_len(b))
    }
}

impl<Buf, T> Serialize for CompactListInBuffer<Buf, T>
where
    Buf: AsRef<[u8]>,
    T: CompactSerialize + CompactSerializeFixedLen,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bytes(self.buf.as_ref())
    }
}

impl<Buf, T> CloneToOwned for CompactListInBuffer<Buf, T>
where
    Buf: CloneToOwned,
{
    type Target = CompactListInBuffer<Buf::Target, T>;

    fn clone_to_owned(&self, within_buffer: Option<&bytes::Bytes>) -> Self::Target {
        CompactListInBuffer {
            buf: self.buf.clone_to_owned(within_buffer),
            _phantom: Default::default(),
        }
    }
}

impl<'de, Buf, T> Deserialize<'de> for CompactListInBuffer<Buf, T>
where
    Buf: Deserialize<'de> + AsRef<[u8]>,
    T: CompactSerialize + CompactSerializeFixedLen,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let buf = Buf::deserialize(deserializer)?;
        // TODO: we could check the len here is the exact multiple, but I don't know
        // how to return the error without creating a custom visitor
        Ok(Self {
            buf,
            _phantom: Default::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fmt::Debug,
        net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
    };

    use bencode::bencode_serialize_to_writer;
    use buffers::ByteBuf;

    use crate::compact_ip::{
        Compact, CompactListInBuffer, CompactListInBufferOwned, CompactSerialize,
        CompactSerializeFixedLen,
    };

    fn test_same_impl<T: CompactSerialize + Copy + Debug + PartialEq + Eq>(input: T) {
        println!("{input:?}");
        let input = Compact(input);
        let mut w = Vec::new();
        bencode_serialize_to_writer(input, &mut w).unwrap();
        println!("input={input:?}, serialized={w:?}");
        let deserialized: Compact<T> = bencode::from_bytes(&w).unwrap();
        assert_eq!(deserialized, input);
    }

    fn test_same_list_impl<
        T: CompactSerialize + CompactSerializeFixedLen + Copy + Debug + PartialEq + Eq,
        const N: usize,
    >(
        input: [T; N],
    ) {
        let l = CompactListInBufferOwned::new_from_iter(input.into_iter());
        let mut w = Vec::new();
        bencode_serialize_to_writer(&l, &mut w).unwrap();
        let deserialized: CompactListInBuffer<ByteBuf, T> = bencode::from_bytes(&w).unwrap();
        assert_eq!(deserialized.iter().collect::<Vec<_>>(), input);
    }

    #[test]
    fn test_same() {
        test_same_list_impl([Ipv4Addr::LOCALHOST, Ipv4Addr::BROADCAST]);
        test_same_list_impl([Ipv6Addr::LOCALHOST, Ipv6Addr::UNSPECIFIED]);
        test_same_list_impl([
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 10),
            SocketAddrV4::new(Ipv4Addr::BROADCAST, 11),
        ]);
        test_same_list_impl([
            SocketAddrV6::new(Ipv6Addr::LOCALHOST, 10, 0, 0),
            SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 11, 0, 0),
        ]);

        test_same_impl(Ipv4Addr::LOCALHOST);
        test_same_impl(Ipv6Addr::LOCALHOST);
        test_same_impl(IpAddr::V4(Ipv4Addr::LOCALHOST));
        test_same_impl(IpAddr::V6(Ipv6Addr::LOCALHOST));
        test_same_impl(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 100));
        test_same_impl(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 100, 0, 0));
        test_same_impl(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 100));
        test_same_impl(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 100));
    }
}
