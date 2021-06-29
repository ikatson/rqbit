use std::{
    collections::{HashMap, HashSet},
    fs::File,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use futures::{stream::FuturesUnordered, StreamExt};
use log::{debug, trace, warn};
use parking_lot::{Mutex, RwLock};
use tokio::sync::mpsc::{channel, Sender};

use crate::{
    chunk_tracker::ChunkTracker,
    file_ops::FileOps,
    lengths::{Lengths, ValidPieceIndex},
    peer_binary_protocol::{Handshake, Message},
    peer_connection::{PeerConnection, WriterRequest},
    peer_state::{LivePeerState, PeerState},
    spawn_utils::spawn,
    torrent_metainfo::TorrentMetaV1Owned,
    type_aliases::{PeerHandle, BF},
};

#[derive(Default)]
pub struct PeerStates {
    states: HashMap<PeerHandle, PeerState>,
    seen: HashSet<SocketAddr>,
    inflight_pieces: HashSet<ValidPieceIndex>,
    tx: HashMap<PeerHandle, Arc<tokio::sync::mpsc::Sender<WriterRequest>>>,
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
        tx: tokio::sync::mpsc::Sender<WriterRequest>,
    ) -> Option<PeerHandle> {
        if self.seen.contains(&addr) {
            return None;
        }
        let handle = self.add(addr, tx)?;
        self.seen.insert(addr);
        Some(handle)
    }
    pub fn seen(&self) -> &HashSet<SocketAddr> {
        &self.seen
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
        tx: tokio::sync::mpsc::Sender<WriterRequest>,
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
        let live = self.get_live_mut(handle)?;
        let prev = live.i_am_choked;
        live.i_am_choked = is_choked;
        Some(prev)
    }
    pub fn mark_peer_interested(
        &mut self,
        handle: PeerHandle,
        is_interested: bool,
    ) -> Option<bool> {
        let live = self.get_live_mut(handle)?;
        let prev = live.peer_interested;
        live.peer_interested = is_interested;
        Some(prev)
    }
    pub fn update_bitfield_from_vec(
        &mut self,
        handle: PeerHandle,
        bitfield: Vec<u8>,
    ) -> Option<Option<BF>> {
        let live = self.get_live_mut(handle)?;
        let bitfield = BF::from_vec(bitfield);
        let prev = live.bitfield.take();
        live.bitfield = Some(bitfield);
        Some(prev)
    }
    pub fn clone_tx(&self, handle: PeerHandle) -> Option<Arc<Sender<WriterRequest>>> {
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
    pub fn file_ops(&self) -> FileOps<'_> {
        FileOps::new(&self.torrent, &self.files, &self.lengths)
    }

    pub fn get_next_needed_piece(&self, peer_handle: PeerHandle) -> Option<ValidPieceIndex> {
        let g = self.locked.read();
        let bf = g.peers.get_live(peer_handle)?.bitfield.as_ref()?;
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
            .get_live(peer_handle)
            .map(|l| l.i_am_choked)
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
                *s = PeerState::Live(LivePeerState::new(h.peer_id));
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

    pub fn maybe_transmit_haves(&self, index: ValidPieceIndex) {
        let mut futures = Vec::new();

        let g = self.locked.read();
        for (handle, peer_state) in g.peers.states.iter() {
            match peer_state {
                PeerState::Live(live) => {
                    if !live.peer_interested {
                        continue;
                    }

                    if live
                        .bitfield
                        .as_ref()
                        .and_then(|b| b.get(index.get() as usize).map(|v| *v))
                        .unwrap_or(false)
                    {
                        continue;
                    }

                    let tx = match g.peers.tx.get(handle) {
                        Some(tx) => tx,
                        None => continue,
                    };
                    let tx = Arc::downgrade(tx);
                    futures.push(async move {
                        if let Some(tx) = tx.upgrade() {
                            if tx
                                .send(WriterRequest::Message(Message::Have(index.get())))
                                .await
                                .is_err()
                            {
                                // whatever
                            }
                        }
                    });
                }
                _ => continue,
            }
        }

        if futures.is_empty() {
            trace!("no peers to transmit Have={} to, saving some work", index);
            return;
        }

        let mut unordered: FuturesUnordered<_> = futures.into_iter().collect();
        spawn(
            format!("transmit_haves(piece={}, count={})", index, unordered.len()),
            async move {
                while unordered.next().await.is_some() {}
                Ok(())
            },
        );
    }

    pub fn add_peer(self: &Arc<Self>, addr: SocketAddr) {
        let (out_tx, out_rx) = channel::<WriterRequest>(1);
        let handle = match self.locked.write().peers.add_if_not_seen(addr, out_tx) {
            Some(handle) => handle,
            None => return,
        };

        let peer_connection = PeerConnection::new(self.clone());
        spawn(format!("manage_peer({})", handle), async move {
            if let Err(e) = peer_connection.manage_peer(addr, handle, out_rx).await {
                debug!("error managing peer {}: {:#}", handle, e)
            };
            peer_connection.into_state().drop_peer(handle);
            Ok::<_, anyhow::Error>(())
        });
    }
}
