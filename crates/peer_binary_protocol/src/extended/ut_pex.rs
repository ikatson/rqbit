use std::net::{IpAddr, SocketAddr};

use byteorder::{ByteOrder, BE};
use bytes::Bytes;
use clone_to_owned::CloneToOwned;
use itertools::{EitherOrBoth, Itertools};
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
    added: B,
    #[serde(rename = "added.f")]
    #[serde(skip_serializing_if = "Option::is_none")]
    added_f: Option<B>,
    added6: B,
    #[serde(rename = "added6.f")]
    #[serde(skip_serializing_if = "Option::is_none")]
    added6_f: Option<B>,
    dropped: B,
    dropped6: B,
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
        buf: &'a B,
        flags: &'a Option<B>,
        ip_len: usize,
    ) -> impl Iterator<Item = PexPeerInfo> + Clone + 'a {
        let addrs = buf.as_ref().chunks_exact(ip_len + 2).map(move |c| {
            let ip = match ip_len {
                4 => IpAddr::from(TryInto::<[u8; 4]>::try_into(&c[..4]).unwrap()),
                16 => IpAddr::from(TryInto::<[u8; 16]>::try_into(&c[..16]).unwrap()),
                _ => unreachable!(),
            };
            let port = BE::read_u16(&c[ip_len..]);
            SocketAddr::new(ip, port)
        });
        let flags = flags
            .as_ref()
            .map(|b| b.as_ref().iter().copied())
            .into_iter()
            .flatten();
        addrs.zip_longest(flags).filter_map(|eob| match eob {
            EitherOrBoth::Both(addr, flags) => Some(PexPeerInfo { flags, addr }),
            EitherOrBoth::Left(addr) => Some(PexPeerInfo { flags: 0, addr }),
            EitherOrBoth::Right(_) => None,
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

#[cfg(test)]
mod tests {
    use bencode::from_bytes;
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
}
