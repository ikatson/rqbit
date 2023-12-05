use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};

use buffers::ByteBuf;
use byteorder::ByteOrder;
use byteorder::BE;
use clone_to_owned::CloneToOwned;
use serde::{Deserialize, Deserializer, Serialize};

use crate::MY_EXTENDED_UT_METADATA;

#[derive(Deserialize, Serialize, Debug, Default)]
pub struct ExtendedHandshake<ByteBuf: Eq + std::hash::Hash> {
    #[serde(bound(deserialize = "ByteBuf: From<&'de [u8]>"))]
    pub m: HashMap<ByteBuf, u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub v: Option<ByteBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yourip: Option<YourIP>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv6: Option<ByteBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv4: Option<ByteBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reqq: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata_size: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub complete_ago: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_only: Option<u32>,
}

impl ExtendedHandshake<ByteBuf<'static>> {
    pub fn new() -> Self {
        let mut features = HashMap::new();
        features.insert(ByteBuf(b"ut_metadata"), MY_EXTENDED_UT_METADATA);
        Self {
            m: features,
            ..Default::default()
        }
    }
}

impl<ByteBuf: Eq + std::hash::Hash> ExtendedHandshake<ByteBuf> {
    pub fn get_msgid(&self, msg_type: &[u8]) -> Option<u8>
    where
        ByteBuf: AsRef<[u8]>,
    {
        self.m.iter().find_map(|(k, v)| {
            if k.as_ref() == msg_type {
                Some(*v)
            } else {
                None
            }
        })
    }

    pub fn ut_metadata(&self) -> Option<u8>
    where
        ByteBuf: AsRef<[u8]>,
    {
        self.get_msgid(b"ut_metadata")
    }
}

impl<ByteBuf> CloneToOwned for ExtendedHandshake<ByteBuf>
where
    ByteBuf: CloneToOwned + Eq + std::hash::Hash,
    <ByteBuf as CloneToOwned>::Target: Eq + std::hash::Hash,
{
    type Target = ExtendedHandshake<<ByteBuf as CloneToOwned>::Target>;

    fn clone_to_owned(&self) -> Self::Target {
        ExtendedHandshake {
            m: self.m.clone_to_owned(),
            p: self.p,
            v: self.v.clone_to_owned(),
            yourip: self.yourip,
            ipv6: self.ipv6.clone_to_owned(),
            ipv4: self.ipv4.clone_to_owned(),
            reqq: self.reqq,
            metadata_size: self.metadata_size,
            complete_ago: self.complete_ago,
            upload_only: self.upload_only,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct YourIP(pub IpAddr);

impl Serialize for YourIP {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.0 {
            IpAddr::V4(ipv4) => {
                let buf = ipv4.octets();
                serializer.serialize_bytes(&buf)
            }
            IpAddr::V6(_) => todo!(),
        }
    }
}

impl<'de> Deserialize<'de> for YourIP {
    fn deserialize<D>(de: D) -> Result<YourIP, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor {}
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = YourIP;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "expecting 4 bytes of ipv4 or 16 bytes of ipv6")
            }

            fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if v.len() == 4 {
                    return Ok(YourIP(IpAddr::V4(Ipv4Addr::new(v[0], v[1], v[2], v[3]))));
                } else if v.len() == 16 {
                    return Ok(YourIP(IpAddr::V6(Ipv6Addr::new(
                        BE::read_u16(&v[..2]),
                        BE::read_u16(&v[2..4]),
                        BE::read_u16(&v[4..6]),
                        BE::read_u16(&v[6..8]),
                        BE::read_u16(&v[8..10]),
                        BE::read_u16(&v[10..12]),
                        BE::read_u16(&v[12..14]),
                        BE::read_u16(&v[14..]),
                    ))));
                }
                Err(E::custom("expected 4 or 16 byte address"))
            }
        }
        de.deserialize_bytes(Visitor {})
    }
}
