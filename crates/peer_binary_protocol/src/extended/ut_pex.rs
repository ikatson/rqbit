use std::net::{IpAddr, SocketAddr};

use buffers::ByteBufOwned;
use byteorder::{ByteOrder, BE};
use bytes::{Bytes, BytesMut};
use clone_to_owned::CloneToOwned;
use serde::{Deserialize, Serialize};

pub struct PexPeerInfo {
    pub flags: u8,
    pub addr: SocketAddr,
}

impl core::fmt::Debug for PexPeerInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.addr)?;
        if self.flags != 0 {
            write!(f, ";flags={}", self.flags)?;
        }
        Ok(())
    }
}

#[derive(Serialize, Default, Deserialize)]
pub struct UtPex<B> {
    #[serde(skip_serializing_if = "Option::is_none")]
    added: Option<B>,
    #[serde(rename = "added.f")]
    #[serde(skip_serializing_if = "Option::is_none")]
    added_f: Option<B>,
    #[serde(skip_serializing_if = "Option::is_none")]
    added6: Option<B>,
    #[serde(rename = "added6.f")]
    #[serde(skip_serializing_if = "Option::is_none")]
    added6_f: Option<B>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dropped: Option<B>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dropped6: Option<B>,
}

impl<B> core::fmt::Debug for UtPex<B>
where
    B: AsRef<[u8]>,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        struct IterDebug<I>(I);
        impl<I> core::fmt::Debug for IterDebug<I>
        where
            I: Iterator<Item = PexPeerInfo> + Clone,
        {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_list().entries(self.0.clone()).finish()
            }
        }
        f.debug_struct("UtPex")
            .field("added", &IterDebug(self.added_peers()))
            .field("dropped", &IterDebug(self.dropped_peers()))
            .finish()
    }
}

impl<B> CloneToOwned for UtPex<B>
where
    B: CloneToOwned,
{
    type Target = UtPex<<B as CloneToOwned>::Target>;
    fn clone_to_owned(&self, within_buffer: Option<&Bytes>) -> Self::Target {
        UtPex {
            added: self.added.clone_to_owned(within_buffer),
            added_f: self.added_f.clone_to_owned(within_buffer),
            added6: self.added6.clone_to_owned(within_buffer),
            added6_f: self.added6_f.clone_to_owned(within_buffer),
            dropped: self.dropped.clone_to_owned(within_buffer),
            dropped6: self.dropped6.clone_to_owned(within_buffer),
        }
    }
}

impl<B> UtPex<B>
where
    B: AsRef<[u8]>,
{
    fn added_peers_inner<'a>(
        &'a self,
        buf: &'a Option<B>,
        flags: &'a Option<B>,
        ip_len: usize,
    ) -> impl Iterator<Item = PexPeerInfo> + Clone + 'a {
        const PORT_LEN: usize = 2;
        const DEFAULT_FLAGS: u8 = 0;
        let addrs = buf
            .as_ref()
            .into_iter()
            .flat_map(move |it| it.as_ref().chunks_exact(ip_len + PORT_LEN))
            .map(move |c| {
                let ip = match ip_len {
                    4 => IpAddr::from(TryInto::<[u8; 4]>::try_into(&c[..4]).unwrap()),
                    16 => IpAddr::from(TryInto::<[u8; 16]>::try_into(&c[..16]).unwrap()),
                    _ => unreachable!(),
                };
                let port = BE::read_u16(&c[ip_len..]);
                SocketAddr::new(ip, port)
            });
        addrs.enumerate().map(move |(id, addr)| PexPeerInfo {
            addr,
            flags: flags
                .as_ref()
                .and_then(|f| f.as_ref().get(id).copied())
                .unwrap_or(DEFAULT_FLAGS),
        })
    }

    pub fn added_peers(&self) -> impl Iterator<Item = PexPeerInfo> + Clone + '_ {
        self.added_peers_inner(&self.added, &self.added_f, 4)
            .chain(self.added_peers_inner(&self.added6, &self.added6_f, 16))
    }

    pub fn dropped_peers(&self) -> impl Iterator<Item = PexPeerInfo> + Clone + '_ {
        self.added_peers_inner(&self.dropped, &None, 4)
            .chain(self.added_peers_inner(&self.dropped6, &None, 16))
    }
}

impl UtPex<ByteBufOwned> {
    pub fn from_addrs<'a, I, J>(addrs_live: I, addrs_closed: J) -> Self
    where
        I: IntoIterator<Item = &'a SocketAddr>,
        J: IntoIterator<Item = &'a SocketAddr>,
    {
        fn addrs_to_bytes<'a, I>(addrs: I) -> (Option<ByteBufOwned>, Option<ByteBufOwned>)
        where
            I: IntoIterator<Item = &'a SocketAddr>,
        {
            let mut ipv4_addrs = BytesMut::new();
            let mut ipv6_addrs = BytesMut::new();
            for addr in addrs {
                match addr {
                    SocketAddr::V4(v4) => {
                        ipv4_addrs.extend_from_slice(&v4.ip().octets());
                        ipv4_addrs.extend_from_slice(&v4.port().to_be_bytes());
                    }
                    SocketAddr::V6(v6) => {
                        ipv6_addrs.extend_from_slice(&v6.ip().octets());
                        ipv6_addrs.extend_from_slice(&v6.port().to_be_bytes());
                    }
                }
            }

            let freeze = |buf: BytesMut| -> Option<ByteBufOwned> {
                if !buf.is_empty() {
                    Some(buf.freeze().into())
                } else {
                    None
                }
            };

            (freeze(ipv4_addrs), freeze(ipv6_addrs))
        }

        let (added, added6) = addrs_to_bytes(addrs_live);
        let (dropped, dropped6) = addrs_to_bytes(addrs_closed);

        Self {
            added,
            added6,
            dropped,
            dropped6,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use bencode::{bencode_serialize_to_writer, from_bytes};
    use buffers::ByteBuf;

    use super::*;

    fn decode_hex(s: &str) -> Vec<u8> {
        assert!(s.len() % 2 == 0);
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn test_pex_deserialization() {
        let msg = "64353a616464656431323ab99f9d14b56797f969861090373a61646465642e66323a0c00363a616464656436303a383a6164646564362e66303a373a64726f70706564303a383a64726f7070656436303a65";
        let bytes = decode_hex(msg);
        let pex = from_bytes::<UtPex<ByteBuf>>(&bytes).unwrap();
        let addrs: Vec<_> = pex.added_peers().collect();
        assert_eq!(2, addrs.len());
        assert_eq!(
            "185.159.157.20:46439".parse::<SocketAddr>().unwrap(),
            addrs[0].addr
        );
        assert_eq!(12, addrs[0].flags);
        assert_eq!(
            "151.249.105.134:4240".parse::<SocketAddr>().unwrap(),
            addrs[1].addr
        );
        assert_eq!(0, addrs[1].flags);
    }

    #[test]
    fn test_pex_roundtrip() {
        let a1 = "185.159.157.20:46439".parse::<SocketAddr>().unwrap();
        let a2 = "151.249.105.134:4240".parse::<SocketAddr>().unwrap();
        //IPV6
        let aa1 = "[5be8:dde9:7f0b:d5a7:bd01:b3be:9c69:573b]:46439"
            .parse::<SocketAddr>()
            .unwrap();
        let aa2 = "[f16c:f7ec:cfa2:e1c5:9a3c:cb08:801f:36b8]:4240"
            .parse::<SocketAddr>()
            .unwrap();

        let addrs = vec![a1, aa1, a2, aa2];
        let pex = UtPex::from_addrs(&addrs, &addrs);
        let mut bytes = Vec::new();
        bencode_serialize_to_writer(&pex, &mut bytes).unwrap();
        let pex2 = from_bytes::<UtPex<ByteBuf>>(&bytes).unwrap();
        assert_eq!(4, pex2.added_peers().count());
        assert_eq!(pex.added_peers().count(), pex2.added_peers().count());
        let addrs2: Vec<_> = pex2.added_peers().collect();
        assert_eq!(a1, addrs2[0].addr);
        assert_eq!(a2, addrs2[1].addr);
        assert_eq!(aa1, addrs2[2].addr);
        assert_eq!(aa2, addrs2[3].addr);
        let addrs2: Vec<_> = pex2.dropped_peers().collect();
        assert_eq!(a1, addrs2[0].addr);
        assert_eq!(a2, addrs2[1].addr);
        assert_eq!(aa1, addrs2[2].addr);
        assert_eq!(aa2, addrs2[3].addr);
    }
}
