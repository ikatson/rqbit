use byteorder::ByteOrder;
use serde::{Deserialize, Deserializer};
use std::{
    fmt::Write,
    marker::PhantomData,
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    str::FromStr,
};

use crate::buffers::ByteBuf;

#[derive(Clone, Copy)]
pub enum TrackerRequestEvent {
    Started,
    Stopped,
    Completed,
}

pub struct TrackerRequest {
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub event: Option<TrackerRequestEvent>,
    pub port: u16,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub compact: bool,
    pub no_peer_id: bool,

    pub ip: Option<std::net::IpAddr>,
    pub numwant: Option<usize>,
    pub key: Option<String>,
    pub trackerid: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct TrackerError<'a> {
    #[serde(rename = "failure reason", borrow)]
    pub failure_reason: ByteBuf<'a>,
}

#[derive(Deserialize, Debug)]
pub struct DictPeer<'a> {
    #[serde(deserialize_with = "deserialize_ip_string")]
    ip: IpAddr,
    #[serde(borrow)]
    peer_id: Option<ByteBuf<'a>>,
    port: u16,
}

impl<'a> DictPeer<'a> {
    fn as_sockaddr(&self) -> SocketAddr {
        SocketAddr::new(self.ip, self.port)
    }
}

#[derive(Debug)]
pub enum Peers<'a> {
    Full(Vec<DictPeer<'a>>),
    Compact(Vec<SocketAddrV4>),
}

impl<'a> Peers<'a> {
    pub fn iter_sockaddrs(&self) -> Box<dyn Iterator<Item = std::net::SocketAddr> + '_> {
        match self {
            Peers::Full(d) => Box::new(d.iter().map(DictPeer::as_sockaddr)),
            Peers::Compact(c) => Box::new(c.iter().copied().map(SocketAddr::V4)),
        }
    }
}

impl<'de: 'a, 'a> serde::de::Deserialize<'de> for Peers<'a> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor<'de> {
            phantom: std::marker::PhantomData<&'de ()>,
        }
        impl<'de> serde::de::Visitor<'de> for Visitor<'de> {
            type Value = Peers<'de>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a list of peers in dict or binary format")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut peers = Vec::new();
                while let Some(peer) = seq.next_element::<DictPeer>()? {
                    peers.push(peer)
                }
                Ok(Peers::Full(peers))
            }

            fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(Peers::Compact(parse_compact_peers(v)))
            }
        }
        deserializer.deserialize_any(Visitor {
            phantom: PhantomData,
        })
    }
}

fn deserialize_ip_string<'de, D>(de: D) -> Result<IpAddr, D::Error>
where
    D: Deserializer<'de>,
{
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = IpAddr;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("expecting an IPv4 address")
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            IpAddr::from_str(v).map_err(|e| E::custom(format!("cannot parse ip: {}", e)))
        }
    }
    de.deserialize_str(Visitor {})
}

fn parse_compact_peers(b: &[u8]) -> Vec<SocketAddrV4> {
    let mut ips = Vec::new();
    for chunk in b.chunks_exact(6) {
        let ip_chunk = &chunk[..4];
        let port_chunk = &chunk[4..6];
        let ipaddr = Ipv4Addr::new(ip_chunk[0], ip_chunk[1], ip_chunk[2], ip_chunk[3]);
        let port = byteorder::BigEndian::read_u16(port_chunk);
        ips.push(SocketAddrV4::new(ipaddr, port));
    }
    ips
}

#[derive(Deserialize, Debug)]
pub struct TrackerResponse<'a> {
    #[serde(rename = "warning message", borrow)]
    pub warning_message: Option<ByteBuf<'a>>,
    pub complete: u64,
    pub interval: u64,
    #[serde(rename = "min interval")]
    pub min_interval: Option<u64>,
    pub tracker_id: Option<ByteBuf<'a>>,
    pub incomplete: u64,
    pub peers: Peers<'a>,
}

impl TrackerRequest {
    pub fn as_querystring(&self) -> String {
        use urlencoding as u;
        let mut s = String::new();
        s.push_str("info_hash=");
        s.push_str(u::encode_binary(&self.info_hash).as_ref());
        s.push_str("&peer_id=");
        s.push_str(u::encode_binary(&self.peer_id).as_ref());
        if let Some(event) = self.event {
            write!(
                s,
                "&event={}",
                match event {
                    TrackerRequestEvent::Started => "started",
                    TrackerRequestEvent::Stopped => "stopped",
                    TrackerRequestEvent::Completed => "completed",
                }
            )
            .unwrap();
        }
        write!(s, "&port={}", self.port).unwrap();
        write!(s, "&uploaded={}", self.uploaded).unwrap();
        write!(s, "&downloaded={}", self.downloaded).unwrap();
        write!(s, "&left={}", self.left).unwrap();
        write!(s, "&compact={}", if self.compact { 1 } else { 0 }).unwrap();
        write!(s, "&no_peer_id={}", if self.no_peer_id { 1 } else { 0 }).unwrap();
        if let Some(ip) = &self.ip {
            write!(s, "&ip={}", ip).unwrap();
        }
        if let Some(numwant) = &self.numwant {
            write!(s, "&numwant={}", numwant).unwrap();
        }
        if let Some(key) = &self.key {
            write!(s, "&key={}", key).unwrap();
        }
        if let Some(trackerid) = &self.trackerid {
            write!(s, "&trackerid={}", trackerid).unwrap();
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_serialize() {
        let info_hash = [
            1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
        ];
        let peer_id = [
            1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
        ];
        let request = TrackerRequest {
            info_hash,
            peer_id,
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left: 1024 * 1024,
            compact: true,
            no_peer_id: false,
            event: Some(TrackerRequestEvent::Started),
            ip: Some("127.0.0.1".parse().unwrap()),
            numwant: None,
            key: None,
            trackerid: None,
        };
        dbg!(request.as_querystring());
    }
}
