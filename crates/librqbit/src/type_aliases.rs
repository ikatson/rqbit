use std::net::SocketAddr;

use futures::stream::BoxStream;

use crate::opened_file::OpenedFile;

pub type BF = bitvec::boxed::BitBox<u8, bitvec::order::Msb0>;

pub type PeerHandle = SocketAddr;
pub type PeerStream = BoxStream<'static, SocketAddr>;
pub(crate) type OpenedFiles = Vec<OpenedFile>;
