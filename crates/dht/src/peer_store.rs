use std::{collections::VecDeque, net::SocketAddr, str::FromStr, sync::atomic::AtomicU32};

use bencode::ByteBufOwned;
use chrono::{DateTime, Utc};
use librqbit_core::{compact_ip::CompactSocketAddr, hash_id::Id20};
use parking_lot::RwLock;
use rand::Rng;
use serde::{
    Deserialize, Serialize,
    ser::{SerializeMap, SerializeStruct},
};
use tracing::trace;

use crate::bprotocol::{AnnouncePeer, Want};

#[derive(Serialize, Deserialize)]
struct StoredToken {
    token: [u8; 4],
    #[serde(serialize_with = "crate::utils::serialize_id20")]
    node_id: Id20,
    addr: SocketAddr,
}

#[derive(Serialize, Deserialize)]
struct StoredPeer {
    addr: SocketAddr,
    time: DateTime<Utc>,
}

pub struct PeerStore {
    // Our node ids (IPv4, IPv6): an announce is stored only when the info hash is close to the id
    // we use in the address family of the announcing node. They move when BEP 42 gives us a new
    // id, see `set_self_id`.
    self_ids: RwLock<(Id20, Id20)>,
    max_remembered_tokens: u32,
    max_remembered_peers: u32,
    max_distance: Id20,
    tokens: RwLock<VecDeque<StoredToken>>,
    peers: dashmap::DashMap<Id20, Vec<StoredPeer>>,
    peers_len: AtomicU32,
}

impl Serialize for PeerStore {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        struct SerializePeers<'a> {
            peers: &'a dashmap::DashMap<Id20, Vec<StoredPeer>>,
        }

        impl Serialize for SerializePeers<'_> {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                let mut m = serializer.serialize_map(None)?;
                for entry in self.peers.iter() {
                    m.serialize_entry(&entry.key().as_string(), &entry.value())?;
                }
                m.end()
            }
        }

        let mut s = serializer.serialize_struct("PeerStore", 7)?;
        s.serialize_field("self_id", &self.self_ids.read().0.as_string())?;
        s.serialize_field("max_remembered_tokens", &self.max_remembered_tokens)?;
        s.serialize_field("max_remembered_peers", &self.max_remembered_peers)?;
        s.serialize_field("max_distance", &self.max_distance.as_string())?;
        s.serialize_field("tokens", &*self.tokens.read())?;
        s.serialize_field("peers", &SerializePeers { peers: &self.peers })?;
        s.serialize_field(
            "peers_len",
            &self.peers_len.load(std::sync::atomic::Ordering::SeqCst),
        )?;
        s.end()
    }
}

impl<'de> Deserialize<'de> for PeerStore {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Tmp {
            self_id: Id20,
            max_remembered_tokens: u32,
            max_remembered_peers: u32,
            max_distance: Id20,
            tokens: VecDeque<StoredToken>,
            peers: dashmap::DashMap<Id20, Vec<StoredPeer>>,
        }

        Tmp::deserialize(deserializer).map(|tmp| Self {
            // The ids are reset by the DHT on startup anyway, see `set_self_id`.
            self_ids: RwLock::new((tmp.self_id, tmp.self_id)),
            max_remembered_tokens: tmp.max_remembered_tokens,
            max_remembered_peers: tmp.max_remembered_peers,
            max_distance: tmp.max_distance,
            tokens: RwLock::new(tmp.tokens),
            peers_len: AtomicU32::new(tmp.peers.iter().map(|e| e.value().len() as u32).sum()),
            peers: tmp.peers,
        })
    }
}

impl PeerStore {
    pub fn new(self_id: Id20) -> Self {
        Self {
            self_ids: RwLock::new((self_id, self_id)),
            max_remembered_tokens: 1000,
            max_remembered_peers: 1000,
            max_distance: Id20::from_str("00000fffffffffffffffffffffffffffffffffff").unwrap(),
            tokens: RwLock::new(VecDeque::new()),
            peers: dashmap::DashMap::new(),
            peers_len: AtomicU32::new(0),
        }
    }

    pub fn gen_token_for(&self, node_id: Id20, addr: SocketAddr) -> [u8; 4] {
        let mut token = [0u8; 4];
        rand::rng().fill_bytes(&mut token);
        let mut tokens = self.tokens.write();
        tokens.push_back(StoredToken {
            token,
            addr,
            node_id,
        });
        if tokens.len() > self.max_remembered_tokens as usize {
            tokens.pop_front();
        }
        token
    }

    /// Sets our node id of one address family, which decides which info hashes are close enough
    /// to us to store peers for. Peers of that family stored for info hashes that are too far
    /// from the new id are dropped: we would not take them now, and they would hold places that
    /// announces near the new id need.
    pub(crate) fn set_self_id(&self, is_v4: bool, id: Id20) {
        {
            let mut ids = self.self_ids.write();
            if is_v4 {
                ids.0 = id;
            } else {
                ids.1 = id;
            }
        }
        self.peers.retain(|info_hash, peers| {
            if info_hash.distance(&id) > self.max_distance {
                peers.retain(|p| p.addr.is_ipv4() != is_v4);
            }
            !peers.is_empty()
        });
        // Counted again rather than decremented: an announce stored meanwhile may not have
        // added itself to the count yet, and the count must not wrap below zero for it.
        let len: usize = self.peers.iter().map(|e| e.value().len()).sum();
        self.peers_len.store(
            u32::try_from(len).unwrap_or(u32::MAX),
            std::sync::atomic::Ordering::SeqCst,
        );
    }

    #[cfg(test)]
    pub(crate) fn self_id(&self, is_v4: bool) -> Id20 {
        let ids = self.self_ids.read();
        if is_v4 { ids.0 } else { ids.1 }
    }

    pub fn store_peer(&self, announce: &AnnouncePeer<ByteBufOwned>, mut addr: SocketAddr) -> bool {
        // If the info_hash in announce is too far away from us, don't store it.
        // If the token doesn't match, don't store it.
        // If we are out of capacity, don't store it.
        // Otherwise, store it.
        let self_id = {
            let ids = self.self_ids.read();
            if addr.is_ipv4() { ids.0 } else { ids.1 }
        };
        if announce.info_hash.distance(&self_id) > self.max_distance {
            trace!("peer store: info_hash too far to store");
            return false;
        }
        if !self.tokens.read().iter().any(|t| {
            t.token[..] == announce.token.as_ref()[..] && t.addr == addr && t.node_id == announce.id
        }) {
            trace!("peer store: can't find this token / addr combination");
            return false;
        }

        if announce.implied_port == 0 {
            addr.set_port(announce.port);
        }

        use dashmap::mapref::entry::Entry;
        let peers_entry = self.peers.entry(announce.info_hash);
        let peers_len = self.peers_len.load(std::sync::atomic::Ordering::SeqCst);
        match peers_entry {
            Entry::Occupied(mut occ) => {
                if let Some(s) = occ.get_mut().iter_mut().find(|s| s.addr == addr) {
                    s.time = Utc::now();
                    return true;
                }
                if peers_len >= self.max_remembered_peers {
                    trace!("peer store: out of capacity");
                    return false;
                }
                occ.get_mut().push(StoredPeer {
                    addr,
                    time: Utc::now(),
                });
            }
            Entry::Vacant(vac) => {
                if peers_len >= self.max_remembered_peers {
                    trace!("peer store: out of capacity");
                    return false;
                }
                vac.insert(vec![StoredPeer {
                    addr,
                    time: Utc::now(),
                }]);
            }
        }

        self.peers_len
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        true
    }

    pub fn get_for_info_hash(&self, info_hash: Id20, want: Want) -> Vec<CompactSocketAddr> {
        if let Some(stored_peers) = self.peers.get(&info_hash) {
            return stored_peers
                .iter()
                .filter(|p| {
                    matches!(
                        (p.addr, want),
                        (SocketAddr::V6(..), Want::V6 | Want::Both)
                            | (SocketAddr::V4(..), Want::V4 | Want::Both)
                    )
                })
                .map(|p| p.addr.into())
                .collect();
        }
        Vec::new()
    }

    #[allow(dead_code)]
    pub fn garbage_collect_peers(&self) {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn announce(store: &PeerStore, info_hash: Id20, addr: SocketAddr) -> bool {
        let id = Id20::new([1; 20]);
        let token = store.gen_token_for(id, addr);
        let announce = AnnouncePeer {
            id,
            implied_port: 1,
            info_hash,
            port: 0,
            token: ByteBufOwned::from(&token[..]),
        };
        store.store_peer(&announce, addr)
    }

    #[test]
    fn moving_the_id_drops_far_peers_of_that_family() {
        let old = Id20::new([0; 20]);
        let store = PeerStore::new(old);
        let v4 = SocketAddr::from_str("1.2.3.4:5").unwrap();
        let v6 = SocketAddr::from_str("[2001:db8::1]:5").unwrap();
        let near_old = Id20::new([0; 20]);
        assert!(announce(&store, near_old, v4));
        assert!(announce(&store, near_old, v6));
        assert_eq!(store.peers_len.load(std::sync::atomic::Ordering::SeqCst), 2);

        // Only the IPv4 id moves, far away: the IPv4 peer goes, the IPv6 one stays.
        let new = Id20::new([0xff; 20]);
        store.set_self_id(true, new);
        assert_eq!(store.peers_len.load(std::sync::atomic::Ordering::SeqCst), 1);
        let left = store.get_for_info_hash(near_old, Want::Both);
        assert_eq!(left.len(), 1);

        // Announces near the new id are taken, near the old one no longer.
        assert!(announce(&store, new, v4));
        assert!(!announce(&store, near_old, v4));
    }
}
