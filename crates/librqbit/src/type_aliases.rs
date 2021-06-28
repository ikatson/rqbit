use std::net::SocketAddr;

pub type BF = bitvec::vec::BitVec<bitvec::order::Msb0, u8>;

pub type PeerHandle = SocketAddr;
