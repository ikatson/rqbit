use std::{collections::HashMap, net::IpAddr};

use buffers::{ByteBuf, ByteBufT};
use bytes::Bytes;
use clone_to_owned::CloneToOwned;
use serde::{Deserialize, Serialize};

use crate::{
    EXTENDED_UT_METADATA_KEY, EXTENDED_UT_PEX_KEY, MY_EXTENDED_UT_METADATA, MY_EXTENDED_UT_PEX,
};

use super::{PeerExtendedMessageIds, PeerIP4, PeerIP6, PeerIPAny};

#[derive(Deserialize, Serialize, Debug, Default)]
pub struct ExtendedHandshake<ByteBuf: ByteBufT> {
    #[serde(bound(deserialize = "ByteBuf: From<&'de [u8]>"))]
    pub m: HashMap<ByteBuf, u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub v: Option<ByteBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yourip: Option<PeerIPAny>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv6: Option<PeerIP6>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv4: Option<PeerIP4>,
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

impl<ByteBuf> ExtendedHandshake<ByteBuf>
where
    ByteBuf: ByteBufT,
{
    fn get_msgid(&self, msg_type: &[u8]) -> Option<u8> {
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

    pub fn ip_addr(&self) -> Option<IpAddr> {
        if let Some(ref b) = self.ipv4 {
            return Some(b.0.into());
        }
        if let Some(ref b) = self.ipv6 {
            return Some(b.0.into());
        }
        None
    }

    pub fn port(&self) -> Option<u16> {
        self.p.and_then(|p| u16::try_from(p).ok())
    }
}

impl<ByteBuf> CloneToOwned for ExtendedHandshake<ByteBuf>
where
    ByteBuf: ByteBufT,
    <ByteBuf as CloneToOwned>::Target: ByteBufT,
{
    type Target = ExtendedHandshake<<ByteBuf as CloneToOwned>::Target>;

    fn clone_to_owned(&self, within_buffer: Option<&Bytes>) -> Self::Target {
        ExtendedHandshake {
            m: self.m.clone_to_owned(within_buffer),
            p: self.p,
            v: self.v.clone_to_owned(within_buffer),
            yourip: self.yourip,
            ipv6: self.ipv6,
            ipv4: self.ipv4,
            reqq: self.reqq,
            metadata_size: self.metadata_size,
            complete_ago: self.complete_ago,
            upload_only: self.upload_only,
        }
    }
}
