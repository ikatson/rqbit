use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use anyhow::Context;
use futures::{stream::FuturesUnordered, StreamExt};
use log::{debug, warn};
use parking_lot::{Mutex, RwLock};
use tokio::sync::{mpsc::Sender, Notify, Semaphore};

use crate::{
    buffers::ByteString,
    chunk_tracker::ChunkTracker,
    file_checking::update_hash_from_file,
    lengths::{ChunkInfo, Lengths, ValidPieceIndex},
    peer_binary_protocol::{Handshake, Message, MessageOwned, Piece},
    peer_state::{LivePeerState, PeerState},
    torrent_metainfo::TorrentMetaV1Owned,
    type_aliases::{PeerHandle, BF},
};

#[derive(Debug, Hash, PartialEq, Eq)]
pub struct InflightRequest {
    pub piece: ValidPieceIndex,
    pub chunk: u32,
}

impl From<&ChunkInfo> for InflightRequest {
    fn from(c: &ChunkInfo) -> Self {
        Self {
            piece: c.piece_index,
            chunk: c.chunk_index,
        }
    }
}

#[derive(Default)]
pub struct PeerStates {
    states: HashMap<PeerHandle, PeerState>,
    seen_peers: HashSet<SocketAddr>,
    inflight_pieces: HashSet<ValidPieceIndex>,
    tx: HashMap<PeerHandle, Arc<tokio::sync::mpsc::Sender<MessageOwned>>>,
}

#[derive(Debug, Default)]
pub struct AggregatePeerStats {
    pub connecting: usize,
    pub live: usize,
}

impl PeerStates {
    pub fn stats(&self) -> AggregatePeerStats {
        self.states
            .values()
            .fold(AggregatePeerStats::default(), |mut s, p| {
                match p {
                    PeerState::Connecting(_) => s.connecting += 1,
                    PeerState::Live(_) => s.live += 1,
                };
                s
            })
    }
    pub fn add_if_not_seen(
        &mut self,
        addr: SocketAddr,
        tx: tokio::sync::mpsc::Sender<MessageOwned>,
    ) -> Option<PeerHandle> {
        if self.seen_peers.contains(&addr) {
            return None;
        }
        let handle = self.add(addr, tx)?;
        self.seen_peers.insert(addr);
        Some(handle)
    }
    pub fn get_live(&self, handle: PeerHandle) -> Option<&LivePeerState> {
        if let PeerState::Live(ref l) = self.states.get(&handle)? {
            return Some(l);
        }
        None
    }
    pub fn get_live_mut(&mut self, handle: PeerHandle) -> Option<&mut LivePeerState> {
        if let PeerState::Live(ref mut l) = self.states.get_mut(&handle)? {
            return Some(l);
        }
        None
    }
    pub fn try_get_live_mut(&mut self, handle: PeerHandle) -> anyhow::Result<&mut LivePeerState> {
        self.get_live_mut(handle)
            .ok_or_else(|| anyhow::anyhow!("peer dropped"))
    }
    pub fn add(
        &mut self,
        addr: SocketAddr,
        tx: tokio::sync::mpsc::Sender<MessageOwned>,
    ) -> Option<PeerHandle> {
        let handle = addr;
        if self.states.contains_key(&addr) {
            return None;
        }
        self.states.insert(handle, PeerState::Connecting(addr));
        self.tx.insert(handle, Arc::new(tx));
        Some(handle)
    }
    pub fn drop_peer(&mut self, handle: PeerHandle) -> Option<PeerState> {
        let result = self.states.remove(&handle);
        self.tx.remove(&handle);
        result
    }
    pub fn mark_i_am_choked(&mut self, handle: PeerHandle, is_choked: bool) -> Option<bool> {
        match self.states.get_mut(&handle) {
            Some(PeerState::Live(live)) => {
                let prev = live.i_am_choked;
                live.i_am_choked = is_choked;
                return Some(prev);
            }
            _ => return None,
        }
    }
    pub fn update_bitfield_from_vec(
        &mut self,
        handle: PeerHandle,
        bitfield: Vec<u8>,
    ) -> Option<Option<BF>> {
        match self.states.get_mut(&handle) {
            Some(PeerState::Live(live)) => {
                let bitfield = BF::from_vec(bitfield);
                let prev = live.bitfield.take();
                live.bitfield = Some(bitfield);
                Some(prev)
            }
            _ => None,
        }
    }
    pub fn clone_tx(&self, handle: PeerHandle) -> Option<Arc<Sender<MessageOwned>>> {
        Some(self.tx.get(&handle)?.clone())
    }
    pub fn remove_inflight_piece(&mut self, piece: ValidPieceIndex) -> bool {
        self.inflight_pieces.remove(&piece)
    }
}

pub struct TorrentStateLocked {
    pub peers: PeerStates,
    pub chunks: ChunkTracker,
}

pub struct AtomicStats {
    pub have: AtomicU64,
    pub downloaded_and_checked: AtomicU64,
    pub uploaded: AtomicU64,
    pub fetched_bytes: AtomicU64,
}

pub struct TorrentState {
    pub torrent: TorrentMetaV1Owned,
    pub locked: Arc<RwLock<TorrentStateLocked>>,
    pub files: Vec<Arc<Mutex<File>>>,
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub lengths: Lengths,
    pub needed: u64,
    pub stats: AtomicStats,
}

impl TorrentState {
    pub fn read_chunk_blocking(
        &self,
        who_sent: PeerHandle,
        chunk_info: ChunkInfo,
    ) -> anyhow::Result<Vec<u8>> {
        let mut absolute_offset = self.lengths.chunk_absolute_offset(&chunk_info);
        let mut result_buf = vec![0u8; chunk_info.size as usize];
        let mut buf = &mut result_buf[..];

        for (file_idx, file_len) in self.torrent.info.iter_file_lengths().enumerate() {
            if absolute_offset > file_len {
                absolute_offset -= file_len;
                continue;
            }
            let file_remaining_len = file_len - absolute_offset;
            let to_read_in_file = std::cmp::min(file_remaining_len, buf.len() as u64) as usize;

            let mut file_g = self.files[file_idx].lock();
            debug!(
                "piece={}, handle={}, file_idx={}, seeking to {}. To read chunk: {:?}",
                chunk_info.piece_index, who_sent, file_idx, absolute_offset, &chunk_info
            );
            file_g
                .seek(SeekFrom::Start(absolute_offset))
                .with_context(|| {
                    format!(
                        "error seeking to {}, file id: {}",
                        absolute_offset, file_idx
                    )
                })?;
            file_g
                .read_exact(&mut buf[..to_read_in_file])
                .with_context(|| {
                    format!(
                        "error reading {} bytes, file_id: {}",
                        file_idx, to_read_in_file
                    )
                })?;

            buf = &mut buf[to_read_in_file..];

            if buf.is_empty() {
                break;
            }

            absolute_offset = 0;
        }

        return Ok(result_buf);
    }

    pub fn get_next_needed_piece(&self, peer_handle: PeerHandle) -> Option<ValidPieceIndex> {
        let g = self.locked.read();
        let bf = match g.peers.states.get(&peer_handle)? {
            PeerState::Live(l) => l.bitfield.as_ref()?,
            _ => return None,
        };
        for n in g.chunks.get_needed_pieces().iter_ones() {
            if bf.get(n).map(|v| *v) == Some(true) {
                // in theory it should be safe without validation, but whatever.
                return self.lengths.validate_piece_index(n as u32);
            }
        }
        None
    }

    pub fn am_i_choked(&self, peer_handle: PeerHandle) -> Option<bool> {
        self.locked
            .read()
            .peers
            .states
            .get(&peer_handle)
            .and_then(|s| match s {
                PeerState::Live(l) => Some(l.i_am_choked),
                _ => None,
            })
    }

    pub fn reserve_next_needed_piece(&self, peer_handle: PeerHandle) -> Option<ValidPieceIndex> {
        if self.am_i_choked(peer_handle)? {
            warn!("we are choked by {}, can't reserve next piece", peer_handle);
            return None;
        }
        let mut g = self.locked.write();
        let n = {
            let mut n_opt = None;
            let bf = g.peers.get_live(peer_handle)?.bitfield.as_ref()?;
            for n in g.chunks.get_needed_pieces().iter_ones() {
                if bf.get(n).map(|v| *v) == Some(true) {
                    n_opt = Some(n);
                    break;
                }
            }

            self.lengths.validate_piece_index(n_opt? as u32)?
        };
        g.peers.inflight_pieces.insert(n);
        g.chunks.reserve_needed_piece(n);
        Some(n)
    }

    pub fn check_piece_blocking(
        &self,
        who_sent: PeerHandle,
        piece_index: ValidPieceIndex,
        last_received_chunk: &ChunkInfo,
    ) -> anyhow::Result<bool> {
        let mut h = sha1::Sha1::new();
        let piece_length = self.lengths.piece_length(piece_index);
        let mut absolute_offset = self.lengths.piece_offset(piece_index);
        let mut buf = vec![0u8; std::cmp::min(65536, piece_length as usize)];

        let mut piece_remaining_bytes = piece_length as usize;

        for (file_idx, (name, file_len)) in
            self.torrent.info.iter_filenames_and_lengths().enumerate()
        {
            if absolute_offset > file_len {
                absolute_offset -= file_len;
                continue;
            }
            let file_remaining_len = file_len - absolute_offset;

            let to_read_in_file =
                std::cmp::min(file_remaining_len, piece_remaining_bytes as u64) as usize;
            let mut file_g = self.files[file_idx].lock();
            debug!(
                "piece={}, handle={}, file_idx={}, seeking to {}. Last received chunk: {:?}",
                piece_index, who_sent, file_idx, absolute_offset, &last_received_chunk
            );
            file_g
                .seek(SeekFrom::Start(absolute_offset))
                .with_context(|| {
                    format!(
                        "error seeking to {}, file id: {}",
                        absolute_offset, file_idx
                    )
                })?;
            update_hash_from_file(&mut file_g, &mut h, &mut buf, to_read_in_file).with_context(
                || {
                    format!(
                        "error reading {} bytes, file_id: {} (\"{:?}\")",
                        to_read_in_file, file_idx, name
                    )
                },
            )?;

            piece_remaining_bytes -= to_read_in_file;

            if piece_remaining_bytes == 0 {
                return Ok(true);
            }

            absolute_offset = 0;
        }

        match self.torrent.info.compare_hash(piece_index.get(), &h) {
            Some(true) => {
                debug!("piece={} hash matches", piece_index);
                Ok(true)
            }
            Some(false) => {
                warn!("the piece={} hash does not match", piece_index);
                Ok(false)
            }
            None => {
                // this is probably a bug?
                warn!("compare_hash() did not find the piece");
                anyhow::bail!("compare_hash() did not find the piece");
            }
        }
    }

    pub fn am_i_interested_in_peer(&self, handle: PeerHandle) -> bool {
        self.get_next_needed_piece(handle).is_some()
    }

    pub fn try_steal_piece(&self, handle: PeerHandle) -> Option<ValidPieceIndex> {
        let mut rng = rand::thread_rng();
        use rand::seq::IteratorRandom;
        let g = self.locked.read();
        let pl = g.peers.get_live(handle)?;
        g.peers
            .inflight_pieces
            .iter()
            .filter(|p| !pl.inflight_requests.iter().any(|req| req.piece == **p))
            .choose(&mut rng)
            .copied()
    }

    pub fn set_peer_live(&self, handle: PeerHandle, h: Handshake) {
        let mut g = self.locked.write();
        match g.peers.states.get_mut(&handle) {
            Some(s @ &mut PeerState::Connecting(_)) => {
                *s = PeerState::Live(LivePeerState {
                    peer_id: h.peer_id,
                    i_am_choked: true,
                    peer_choked: true,
                    peer_interested: false,
                    bitfield: None,
                    have_notify: Arc::new(Notify::new()),
                    outstanding_requests: Arc::new(Semaphore::new(0)),
                    inflight_requests: Default::default(),
                });
            }
            _ => {
                warn!("peer {} was in wrong state", handle);
            }
        }
    }

    pub fn drop_peer(&self, handle: PeerHandle) -> bool {
        let mut g = self.locked.write();
        let peer = match g.peers.drop_peer(handle) {
            Some(peer) => peer,
            None => return false,
        };
        match peer {
            PeerState::Connecting(_) => {}
            PeerState::Live(l) => {
                for req in l.inflight_requests {
                    g.chunks.mark_chunk_request_cancelled(req.piece, req.chunk);
                }
            }
        }
        true
    }

    pub fn get_uploaded(&self) -> u64 {
        self.stats.uploaded.load(Ordering::Relaxed)
    }
    pub fn get_downloaded(&self) -> u64 {
        self.stats.downloaded_and_checked.load(Ordering::Relaxed)
    }

    pub fn get_left_to_download(&self) -> u64 {
        self.needed - self.get_downloaded()
    }

    pub fn write_chunk_blocking(
        &self,
        who_sent: PeerHandle,
        data: &Piece<ByteString>,
        chunk_info: &ChunkInfo,
    ) -> anyhow::Result<()> {
        let mut buf = data.block.as_ref();
        let mut absolute_offset = self.lengths.chunk_absolute_offset(&chunk_info);

        for (file_idx, (name, file_len)) in
            self.torrent.info.iter_filenames_and_lengths().enumerate()
        {
            if absolute_offset > file_len {
                absolute_offset -= file_len;
                continue;
            }

            let remaining_len = file_len - absolute_offset;
            let to_write = std::cmp::min(buf.len(), remaining_len as usize);

            let mut file_g = self.files[file_idx].lock();
            debug!(
                "piece={}, chunk={:?}, handle={}, begin={}, file={}, writing {} bytes at {}",
                chunk_info.piece_index,
                chunk_info,
                who_sent,
                chunk_info.offset,
                file_idx,
                to_write,
                absolute_offset
            );
            file_g
                .seek(SeekFrom::Start(absolute_offset))
                .with_context(|| {
                    format!(
                        "error seeking to {} in file {} (\"{:?}\")",
                        absolute_offset, file_idx, name
                    )
                })?;
            file_g
                .write_all(&buf[..to_write])
                .with_context(|| format!("error writing to file {} (\"{:?}\")", file_idx, name))?;
            buf = &buf[to_write..];
            if buf.is_empty() {
                break;
            }

            absolute_offset = 0;
        }

        Ok(())
    }

    // TODO: this is a task per chunk, not good
    pub async fn task_transmit_haves(&self, index: u32) -> anyhow::Result<()> {
        let mut unordered = FuturesUnordered::new();

        for weak in self
            .locked
            .read()
            .peers
            .tx
            .values()
            .map(|v| Arc::downgrade(v))
        {
            unordered.push(async move {
                if let Some(tx) = weak.upgrade() {
                    if tx.send(Message::Have(index)).await.is_err() {
                        // whatever
                    }
                }
            });
        }

        while unordered.next().await.is_some() {}
        Ok(())
    }
}
