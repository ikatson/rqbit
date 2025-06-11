use buffers::ByteBuf;
use serde::{Deserialize, Deserializer};
use std::{
    marker::PhantomData,
    net::{IpAddr, SocketAddr},
    str::FromStr,
};

use librqbit_core::hash_id::Id20;

#[derive(Clone, Copy)]
pub enum TrackerRequestEvent {
    Started,
    #[allow(dead_code)]
    Stopped,
    #[allow(dead_code)]
    Completed,
}

pub struct TrackerRequest {
    pub info_hash: Id20,
    pub peer_id: Id20,
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
    #[allow(dead_code)]
    peer_id: Option<ByteBuf<'a>>,
    port: u16,
}

impl DictPeer<'_> {
    fn as_sockaddr(&self) -> SocketAddr {
        SocketAddr::new(self.ip, self.port)
    }
}

#[derive(Debug, Default)]
pub struct Peers<const IPV6: bool> {
    addrs: Vec<SocketAddr>,
}

impl<'de, const IPV6: bool> serde::de::Deserialize<'de> for Peers<IPV6> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor<'de, const IPV6: bool> {
            phantom: std::marker::PhantomData<&'de ()>,
        }
        impl<'de, const IPV6: bool> serde::de::Visitor<'de> for Visitor<'de, IPV6> {
            type Value = Peers<IPV6>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a list of peers in dict or binary format")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut peers = Vec::new();
                while let Some(peer) = seq.next_element::<DictPeer>()? {
                    peers.push(peer.as_sockaddr())
                }
                Ok(Peers { addrs: peers })
            }

            fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(Peers {
                    addrs: parse_compact_peers::<IPV6>(v),
                })
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
    impl serde::de::Visitor<'_> for Visitor {
        type Value = IpAddr;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("expecting an IPv4 address")
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            IpAddr::from_str(v).map_err(|e| E::custom(format!("cannot parse ip: {e}")))
        }
    }
    de.deserialize_str(Visitor {})
}

fn parse_compact_peers<const IPV6: bool>(b: &[u8]) -> Vec<SocketAddr> {
    let mut ips = Vec::new();
    const PORT_LEN: usize = 2;
    let ip_len: usize = if IPV6 { 16 } else { 4 };
    for chunk in b.chunks_exact(ip_len + PORT_LEN) {
        let addr = if IPV6 {
            let ip = IpAddr::from(TryInto::<[u8; 16]>::try_into(&chunk[..16]).unwrap());
            let port = u16::from_be_bytes(chunk[16..18].try_into().unwrap());
            SocketAddr::new(ip, port)
        } else {
            let ip = IpAddr::from(TryInto::<[u8; 4]>::try_into(&chunk[..4]).unwrap());
            let port = u16::from_be_bytes(chunk[4..6].try_into().unwrap());
            SocketAddr::new(ip, port)
        };
        ips.push(addr);
    }
    ips
}

#[derive(Deserialize, Debug)]
pub struct TrackerResponse<'a> {
    #[allow(dead_code)]
    #[serde(rename = "warning message", borrow)]
    pub warning_message: Option<ByteBuf<'a>>,
    #[allow(dead_code)]
    pub complete: u64,
    pub interval: u64,
    #[allow(dead_code)]
    #[serde(rename = "min interval")]
    pub min_interval: Option<u64>,
    #[allow(dead_code)]
    pub tracker_id: Option<ByteBuf<'a>>,
    #[allow(dead_code)]
    pub incomplete: u64,
    pub peers: Peers<false>,
    #[serde(default)]
    pub peers6: Peers<true>,
}

impl TrackerResponse<'_> {
    pub fn iter_peers(&self) -> impl Iterator<Item = SocketAddr> {
        self.peers
            .addrs
            .iter()
            .copied()
            .chain(self.peers6.addrs.iter().copied())
    }
}

impl TrackerRequest {
    pub fn as_querystring(&self) -> String {
        use std::fmt::Write;
        use urlencoding as u;
        let mut s = String::new();
        s.push_str("info_hash=");
        s.push_str(u::encode_binary(&self.info_hash.0).as_ref());
        s.push_str("&peer_id=");
        s.push_str(u::encode_binary(&self.peer_id.0).as_ref());
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
            write!(s, "&ip={ip}").unwrap();
        }
        if let Some(numwant) = &self.numwant {
            write!(s, "&numwant={numwant}").unwrap();
        }
        if let Some(key) = &self.key {
            write!(s, "&key={key}").unwrap();
        }
        if let Some(trackerid) = &self.trackerid {
            write!(s, "&trackerid={trackerid}").unwrap();
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_serialize() {
        let info_hash = Id20::new([
            1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
        ]);
        let peer_id = Id20::new([
            1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
        ]);
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
