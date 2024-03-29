use std::net::SocketAddr;

use futures::stream::BoxStream;

pub type BF = bitvec::vec::BitVec<u8, bitvec::order::Msb0>;

pub type PeerHandle = SocketAddr;
pub type PeerStream = BoxStream<'static, SocketAddr>;
