use std::{
    collections::VecDeque,
    net::{SocketAddr, SocketAddrV4},
    str::FromStr,
    sync::atomic::AtomicU64,
    time::Instant,
};

use bencode::ByteString;
use librqbit_core::id20::Id20;
use parking_lot::RwLock;
use rand::RngCore;
use tracing::trace;

use crate::bprotocol::{AnnouncePeer, CompactPeerInfo, Response};

struct StoredToken {
    token: [u8; 4],
    node_id: Id20,
    addr: SocketAddr,
}

struct StoredPeer {
    addr: SocketAddrV4,
    time: Instant,
}

pub struct PeerStore {
    self_id: Id20,
    max_remembered_tokens: usize,
    max_remembered_peers: usize,
    max_distance: Id20,
    tokens: RwLock<VecDeque<StoredToken>>,
    peers: dashmap::DashMap<Id20, Vec<StoredPeer>>,
    peers_len: AtomicU64,
}

impl PeerStore {
    pub fn new(self_id: Id20) -> Self {
        Self {
            self_id,
            max_remembered_tokens: 1000,
            max_remembered_peers: 1000,
            max_distance: Id20::from_str("00000fffffffffffffffffffffffffffffffffff").unwrap(),
            tokens: RwLock::new(VecDeque::new()),
            peers: dashmap::DashMap::new(),
            peers_len: AtomicU64::new(0),
        }
    }

    pub fn gen_token_for(&self, node_id: Id20, addr: SocketAddr) -> [u8; 4] {
        let mut token = [0u8; 4];
        rand::thread_rng().fill_bytes(&mut token);
        let mut tokens = self.tokens.write();
        tokens.push_back(StoredToken {
            token,
            node_id,
            addr,
        });
        if tokens.len() > self.max_remembered_tokens {
            tokens.pop_front();
        }
        token
    }

    pub fn store_peer(&self, announce: &AnnouncePeer<ByteString>, addr: SocketAddr) -> bool {
        // If the info_hash in announce is too far away from us, don't store it.
        // If the token doesn't match, don't store it.
        // If we are out of capacity, don't store it.
        // Otherwise, store it.
        let mut addr = match addr {
            SocketAddr::V4(addr) => addr,
            SocketAddr::V6(_) => {
                trace!("peer store: IPv6 not supported");
                return false;
            }
        };
        if self.peers_len.load(std::sync::atomic::Ordering::SeqCst)
            >= self.max_remembered_peers as u64
        {
            trace!("peer store: out of capacity");
            return false;
        }

        if announce.info_hash.distance(&self.self_id) > self.max_distance {
            trace!("peer store: info_hash too far to store");
            return false;
        }
        if !self
            .tokens
            .read()
            .iter()
            .any(|t| t.token[..] == announce.token[..] && t.addr == std::net::SocketAddr::V4(addr))
        {
            trace!("peer store: can't find this token / addr combination");
            return false;
        }
        if announce.implied_port == 0 {
            addr.set_port(announce.port);
        }
        self.peers
            .entry(announce.info_hash)
            .or_default()
            .push(StoredPeer {
                addr,
                time: Instant::now(),
            });
        self.peers_len
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        true
    }

    pub fn get_for_info_hash(&self, info_hash: Id20) -> Vec<CompactPeerInfo> {
        if let Some(stored_peers) = self.peers.get(&info_hash) {
            return stored_peers
                .iter()
                .map(|p| CompactPeerInfo { addr: p.addr })
                .collect();
        }
        Vec::new()
    }

    pub fn garbage_collect_peers(&self) {
        todo!()
    }
}
