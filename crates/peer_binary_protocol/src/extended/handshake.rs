use std::net::IpAddr;

use buffers::{ByteBuf, ByteBufT};
use librqbit_core::compact_ip::{CompactIpAddr, CompactIpV4, CompactIpV6};
use serde_derive::{Deserialize, Serialize};

use super::PeerExtendedMessageIds;

#[derive(Deserialize, Serialize, Debug, Default, Eq, PartialEq)]
pub struct ExtendedHandshake<ByteBuf: ByteBufT> {
    pub m: PeerExtendedMessageIds,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub v: Option<ByteBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yourip: Option<CompactIpAddr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv6: Option<CompactIpV6>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv4: Option<CompactIpV4>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reqq: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata_size: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub complete_ago: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_only: Option<u32>,
}

impl ExtendedHandshake<ByteBuf<'_>> {
    pub fn new() -> Self {
        Self {
            m: PeerExtendedMessageIds::my(),
            ..Default::default()
        }
    }

    pub fn peer_extended_messages(&self) -> PeerExtendedMessageIds {
        self.m
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
