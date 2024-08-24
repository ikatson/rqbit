use std::{
    collections::HashMap,
    net::IpAddr,
};

use buffers::ByteBuf;
use bytes::Bytes;
use clone_to_owned::CloneToOwned;
use serde::{Deserialize, Deserializer, Serialize};

use crate::{EXTENDED_UT_METADATA_KEY, EXTENDED_UT_PEX_KEY, MY_EXTENDED_UT_METADATA, MY_EXTENDED_UT_PEX};

use super::PeerExtendedMessageIds;

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
        features.insert(ByteBuf(EXTENDED_UT_METADATA_KEY), MY_EXTENDED_UT_METADATA);
        features.insert(ByteBuf(EXTENDED_UT_PEX_KEY), MY_EXTENDED_UT_PEX);
        Self {
            m: features,
            ..Default::default()
        }
    }
}

impl<'a, ByteBuf> ExtendedHandshake<ByteBuf>
where
    ByteBuf: Eq + std::hash::Hash + std::borrow::Borrow<[u8]>,
{
    fn get_msgid(&self, msg_type: &'a [u8]) -> Option<u8> {
        self.m.get(msg_type).copied()
    }

    pub fn ut_metadata(&self) -> Option<u8> {
        self.get_msgid(EXTENDED_UT_METADATA_KEY)
    }

    pub fn ut_pex(&self) -> Option<u8> {
        self.get_msgid(EXTENDED_UT_PEX_KEY)
    }

    pub fn peer_extended_messages(&self) -> PeerExtendedMessageIds {
        PeerExtendedMessageIds {
            ut_metadata: self.ut_metadata(),
            ut_pex: self.ut_pex(),
        }
    }
}

impl<ByteBuf> CloneToOwned for ExtendedHandshake<ByteBuf>
where
    ByteBuf: CloneToOwned + Eq + std::hash::Hash,
    <ByteBuf as CloneToOwned>::Target: Eq + std::hash::Hash,
{
    type Target = ExtendedHandshake<<ByteBuf as CloneToOwned>::Target>;

    fn clone_to_owned(&self, within_buffer: Option<&Bytes>) -> Self::Target {
        ExtendedHandshake {
            m: self.m.clone_to_owned(within_buffer),
            p: self.p,
            v: self.v.clone_to_owned(within_buffer),
            yourip: self.yourip,
            ipv6: self.ipv6.clone_to_owned(within_buffer),
            ipv4: self.ipv4.clone_to_owned(within_buffer),
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
            IpAddr::V6(ipv6) => {
                let buf = ipv6.octets();
                serializer.serialize_bytes(&buf)
            },
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
                    let ip_bytes: &[u8; 4] = v[0..4].try_into().unwrap(); // Safe to unwrap as we check slice length
                    return Ok(YourIP(IpAddr::from(*ip_bytes)));
                } else if v.len() == 16 {
                    let ip_bytes: &[u8; 16] = v[0..16].try_into().unwrap(); // Safe to unwrap as we check slice length
                    return Ok(YourIP(IpAddr::from(*ip_bytes)));
                }
                Err(E::custom("expected 4 or 16 byte address"))
            }
        }
        de.deserialize_bytes(Visitor {})
    }
}
