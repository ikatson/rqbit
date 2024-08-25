use std::net::{IpAddr, SocketAddr};

use byteorder::{ByteOrder, BE};
use bytes::Bytes;
use clone_to_owned::CloneToOwned;
use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub struct PexPeerInfo {
    pub flags: u8,
    pub addr: SocketAddr,
}

impl PexPeerInfo {
    pub fn from_bytes(buf: &[u8], flags: Option<u8>) -> anyhow::Result<Self> {
        let (ip, port) = match buf.len() {
            6 => {
                let ip_bytes: &[u8; 4] = (&buf[0..4]).try_into()?;
                let ip = IpAddr::from(*ip_bytes);
                let port = BE::read_u16(&buf[4..6]);
                (ip, port)
            }
            18 => {
                let ip_bytes: &[u8; 16] = (&buf[0..16]).try_into()?;
                let ip = IpAddr::from(*ip_bytes);
                let port = BE::read_u16(&buf[16..18]);
                (ip, port)
            }
            _ => anyhow::bail!("invalid pex peer info"),
        };
        Ok(Self {
            flags: flags.unwrap_or(0),
            addr: (ip, port).into(),
        })
    }
}

#[derive(Debug, Serialize, Default, Deserialize)]
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

pub struct PeerAddrIterator<'a> {
    addrs: &'a [u8],
    flags: &'a [u8],
    offset: usize,
    addr_size: usize,
}



impl<'a> Iterator for PeerAddrIterator<'a> {
    type Item = PexPeerInfo;
    fn next(&mut self) -> Option<Self::Item> {
        if self.offset*self.addr_size >= self.addrs.len() {
            return None;
        }

        let addr = &self.addrs[self.offset*self.addr_size..(self.offset+1)*self.addr_size];
        let flags = self.flags.get(self.offset);
        self.offset += 1;
        Some(PexPeerInfo::from_bytes(addr, flags.cloned()).unwrap()) // safe to unwrap as we assure slice length


    }
}

impl<B> UtPex<B>
where
    B: AsRef<[u8]>,
{
    pub fn added_peers<'a>(&'a self) -> anyhow::Result<Box<dyn Iterator<Item = PexPeerInfo> + 'a>> {
        if let Some(added) = &self.added {
            if added.as_ref().len() % 6 != 0 {
                anyhow::bail!("invalid pex added peers");
            }
            return Ok(Box::new(PeerAddrIterator {
                addrs: added.as_ref(),
                flags: self.added_f.as_ref().map(|f| f.as_ref()).unwrap_or(&[]),
                offset: 0,
                addr_size: 6,
            }));
        } else {
            return Ok(Box::new(std::iter::empty()));
        };
    }

    pub fn added_peers_v6<'a>(&'a self) -> anyhow::Result<Box<dyn Iterator<Item = PexPeerInfo> + 'a>> {
        if let Some(added) = &self.added6 {
            if added.as_ref().len() % 18 != 0 {
                anyhow::bail!("invalid pex added6 peers");
            }
            return Ok(Box::new(PeerAddrIterator {
                addrs: added.as_ref(),
                flags: self.added6_f.as_ref().map(|f| f.as_ref()).unwrap_or(&[]),
                offset: 0,
                addr_size: 18,
            }));
        } else {
            return Ok(Box::new(std::iter::empty()));
        };
    }

    pub fn dropped_peers<'a>(&'a self) -> anyhow::Result<Box<dyn Iterator<Item = PexPeerInfo> + 'a>> {
        if let Some(dropped) = &self.dropped {
            if dropped.as_ref().len() % 6 != 0 {
                anyhow::bail!("invalid pex dropped peers");
            }
            return Ok(Box::new(PeerAddrIterator {
                addrs: dropped.as_ref(),
                flags: &[],
                offset: 0,
                addr_size: 6,
            }));
        } else {
            return Ok(Box::new(std::iter::empty()));
        };
    }

    pub fn dropped_peers_v6<'a>(&'a self) -> anyhow::Result<Box<dyn Iterator<Item = PexPeerInfo> + 'a>> {
        if let Some(dropped) = &self.dropped6 {
            if dropped.as_ref().len() % 18 != 0 {
                anyhow::bail!("invalid pex dropped6 peers");
            }
            return Ok(Box::new(PeerAddrIterator {
                addrs: dropped.as_ref(),
                flags: &[],
                offset: 0,
                addr_size: 18,
            }));
        } else {
            return Ok(Box::new(std::iter::empty()));
        };
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
        let addrs: Vec<_> = pex.added_peers().unwrap().collect();
        assert_eq!(2, addrs.len());
        assert_eq!("185.159.157.20:46439".parse::<SocketAddr>().unwrap(), addrs[0].addr);
        assert_eq!(12, addrs[0].flags);
        assert_eq!("151.249.105.134:4240".parse::<SocketAddr>().unwrap(), addrs[1].addr);
        assert_eq!(0, addrs[1].flags);
    }
}
