use std::net::SocketAddr;

use futures::Stream;

pub type BF = bitvec::vec::BitVec<u8, bitvec::order::Msb0>;

pub type PeerHandle = SocketAddr;
pub type PeerStream = Box<dyn Stream<Item = SocketAddr> + Unpin + Send + Sync + 'static>;
