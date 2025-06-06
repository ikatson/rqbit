use std::net::SocketAddr;

use futures::stream::BoxStream;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{file_info::FileInfo, storage::TorrentStorage};

// NOTE: Msb0 is used because that's what bittorrent protocol uses for bitfield.
// Don't change to Lsb0 even though it might be a bit faster (in theory) on LE architectures.
pub type BS = bitvec::slice::BitSlice<u8, bitvec::order::Msb0>;
pub type BF = bitvec::boxed::BitBox<u8, bitvec::order::Msb0>;

pub type PeerHandle = SocketAddr;
pub type PeerStream = BoxStream<'static, SocketAddr>;
pub type FileInfos = Vec<FileInfo>;
pub(crate) type FileStorage = Box<dyn TorrentStorage>;
pub(crate) type FilePriorities = Vec<usize>;

pub(crate) type DiskWorkQueueItem = Box<dyn FnOnce() + Send + Sync>;
pub(crate) type DiskWorkQueueSender = tokio::sync::mpsc::Sender<DiskWorkQueueItem>;

pub(crate) type BoxAsyncRead = Box<dyn AsyncRead + Unpin + Send + 'static>;
pub(crate) type BoxAsyncWrite = Box<dyn AsyncWrite + Unpin + Send + 'static>;
