//! The address book: which synthetic [`SocketAddr`] stands for which I2P peer.

use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
};

use librqbit_core::i2p::{I2pDestination, I2pHash, synthetic_addr, synthetic_index};
use parking_lot::Mutex;

/// How many peers the book remembers. Past this the oldest entries are forgotten; their
/// addresses are never handed out again, so a forgotten address can't reach another peer.
pub(crate) const DEFAULT_CAPACITY: usize = 10_000;

struct Entry {
    hash: I2pHash,
    /// Known when the peer came with its full destination (an incoming stream, a
    /// non-compact tracker response, a naming lookup); otherwise it is looked up by b32 name.
    destination: Option<I2pDestination>,
}

struct Inner {
    by_hash: HashMap<I2pHash, u64>,
    by_index: HashMap<u64, Entry>,
    /// Indexes from the oldest to the newest.
    order: VecDeque<u64>,
    next_index: u64,
}

/// Maps I2P peers (by the hash of their destination) to synthetic addresses in `0100::/64`,
/// for the lifetime of a session. The same peer always gets the same address while it is in
/// the book.
pub(crate) struct AddressBook {
    capacity: usize,
    inner: Mutex<Inner>,
}

impl AddressBook {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            inner: Mutex::new(Inner {
                by_hash: HashMap::new(),
                by_index: HashMap::new(),
                order: VecDeque::new(),
                // 0 is left out: `[100::]` reads like a network address.
                next_index: 1,
            }),
        }
    }

    /// The address of the peer with this hash, adding it if needed.
    pub fn insert_hash(&self, hash: I2pHash) -> SocketAddr {
        self.insert(hash, None)
    }

    /// The address of the peer with this destination, adding it if needed.
    pub fn insert_destination(&self, destination: I2pDestination) -> SocketAddr {
        self.insert(destination.hash(), Some(destination))
    }

    fn insert(&self, hash: I2pHash, destination: Option<I2pDestination>) -> SocketAddr {
        let mut g = self.inner.lock();
        if let Some(&index) = g.by_hash.get(&hash) {
            if let Some(d) = destination
                && let Some(e) = g.by_index.get_mut(&index)
            {
                e.destination.get_or_insert(d);
            }
            return synthetic_addr(index);
        }
        while g.order.len() >= self.capacity {
            let Some(oldest) = g.order.pop_front() else {
                break;
            };
            if let Some(e) = g.by_index.remove(&oldest) {
                g.by_hash.remove(&e.hash);
            }
        }
        let index = g.next_index;
        g.next_index += 1;
        g.by_hash.insert(hash, index);
        g.by_index.insert(index, Entry { hash, destination });
        g.order.push_back(index);
        synthetic_addr(index)
    }

    /// Records the destination of a peer known by hash (after a naming lookup).
    pub fn set_destination(&self, addr: SocketAddr, destination: I2pDestination) {
        let Some(index) = synthetic_index(&addr) else {
            return;
        };
        let mut g = self.inner.lock();
        if let Some(e) = g.by_index.get_mut(&index)
            && e.hash == destination.hash()
        {
            e.destination = Some(destination);
        }
    }

    /// The peer behind a synthetic address: its hash, and its destination when known.
    pub fn lookup(&self, addr: &SocketAddr) -> Option<(I2pHash, Option<I2pDestination>)> {
        let index = synthetic_index(addr)?;
        let g = self.inner.lock();
        let e = g.by_index.get(&index)?;
        Some((e.hash, e.destination.clone()))
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner.lock().order.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use librqbit_core::i2p::is_synthetic;

    fn dest(seed: u8) -> I2pDestination {
        let mut d = vec![seed; 384];
        d.extend_from_slice(&[5, 0, 4, 0, 7, 0, 0]);
        I2pDestination::from_bytes(Bytes::from(d)).unwrap()
    }

    #[test]
    fn stable_addresses() {
        let book = AddressBook::new(DEFAULT_CAPACITY);
        let a = book.insert_hash(dest(1).hash());
        let b = book.insert_destination(dest(2));
        assert_ne!(a, b);
        assert!(is_synthetic(&a) && is_synthetic(&b));
        // Same peer, same address, whichever way it comes.
        assert_eq!(book.insert_destination(dest(1)), a);
        assert_eq!(book.insert_hash(dest(2).hash()), b);
        // The destination learnt later is kept.
        assert_eq!(book.lookup(&a), Some((dest(1).hash(), Some(dest(1)))));
        assert_eq!(book.len(), 2);
    }

    #[test]
    fn destinations_learnt_by_lookup() {
        let book = AddressBook::new(DEFAULT_CAPACITY);
        let a = book.insert_hash(dest(1).hash());
        assert_eq!(book.lookup(&a), Some((dest(1).hash(), None)));
        // A destination that does not match the hash is not recorded.
        book.set_destination(a, dest(2));
        assert_eq!(book.lookup(&a), Some((dest(1).hash(), None)));
        book.set_destination(a, dest(1));
        assert_eq!(book.lookup(&a), Some((dest(1).hash(), Some(dest(1)))));
    }

    #[test]
    fn bounded_and_never_reused() {
        let book = AddressBook::new(3);
        let addrs: Vec<_> = (0..5).map(|i| book.insert_destination(dest(i))).collect();
        assert_eq!(book.len(), 3);
        // The two oldest are forgotten: their addresses lead nowhere.
        assert_eq!(book.lookup(&addrs[0]), None);
        assert_eq!(book.lookup(&addrs[1]), None);
        assert_eq!(book.lookup(&addrs[4]).map(|(h, _)| h), Some(dest(4).hash()));
        // A forgotten peer that comes back gets a new address, never an old one.
        let again = book.insert_destination(dest(0));
        assert!(!addrs.contains(&again));
        assert_eq!(book.len(), 3);
    }

    #[test]
    fn real_addresses_are_not_in_the_book() {
        let book = AddressBook::new(DEFAULT_CAPACITY);
        book.insert_destination(dest(1));
        for real in ["1.0.0.1:6881", "[::1]:6881", "[100:0:0:1::1]:6881"] {
            assert_eq!(book.lookup(&real.parse().unwrap()), None, "{real}");
        }
    }
}
