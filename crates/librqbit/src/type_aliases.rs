use std::net::SocketAddr;

pub type BF = bitvec::vec::BitVec<u8, bitvec::order::Msb0>;

pub type PeerHandle = SocketAddr;
