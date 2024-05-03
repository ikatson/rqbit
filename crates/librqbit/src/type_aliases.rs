use std::net::SocketAddr;

use futures::stream::BoxStream;

use crate::{file_info::FileInfo, storage::TorrentStorage};

pub type BF = bitvec::boxed::BitBox<u8, bitvec::order::Msb0>;

pub type PeerHandle = SocketAddr;
pub type PeerStream = BoxStream<'static, SocketAddr>;
pub type FileInfos = Vec<FileInfo>;
pub(crate) type FileStorage = Box<dyn TorrentStorage>;
pub(crate) type FilePriorities = Vec<usize>;

pub(crate) type DiskWorkQueueItem = Box<dyn FnOnce() + Send + Sync>;
pub(crate) type DiskWorkQueueSender = tokio::sync::mpsc::Sender<DiskWorkQueueItem>;
