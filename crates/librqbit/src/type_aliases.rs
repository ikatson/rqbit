use std::net::SocketAddr;

pub type BF = bitvec::vec::BitVec<bitvec::order::Msb0, u8>;

pub type PeerHandle = SocketAddr;

#[cfg(feature = "sha1-openssl")]
pub type Sha1 = crate::sha1w::Sha1Openssl;

#[cfg(feature = "sha1-system")]
pub type Sha1 = crate::sha1w::Sha1System;

#[cfg(feature = "sha1-rust")]
pub type Sha1 = crate::sha1w::Sha1Rust;
