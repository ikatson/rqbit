//! BitTorrent v2 (BEP 52) hash requests: answering peers from the piece layers we know, and
//! fetching from peers the piece layers we lack (a torrent added without `piece layers`).
//!
//! Answering: a request is checked against the tree of its file and answered from a
//! [`PieceLayerTree`] kept in a small cache, or rejected. Every peer has a budget of answers
//! (a token bucket); a peer that keeps asking past it is rejected, then disconnected.
//!
//! Fetching: the layer of every file that lacks one is asked for in chunks of
//! [`HASH_REQUEST_CHUNK`] hashes with the proof up to the file's `pieces root`, like libtorrent
//! does, from the live peers that negotiated v2. Every answer is checked against the root;
//! a forged one is a protocol error of its peer. A rejected or timed-out chunk goes to another
//! peer; when every peer turned it down it waits [`RETRY_BACKOFF`] before asking them again.
//! Once a whole layer is known it is set on the torrent's info (so its pieces can be
//! verified, hence downloaded) and the stored `.torrent` is rewritten with the layers.

use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use buffers::ByteBufOwned;
use bytes::Bytes;
use librqbit_core::{
    hash_id::Id32,
    merkle::{self, PieceLayerTree},
    metainfo_v2::PieceLayers,
    torrent_metainfo::ValidatedTorrentMetaV1Info,
};
use parking_lot::Mutex;
use peer_binary_protocol::{HashRequest, Hashes, MAX_HASHES_PER_MESSAGE, Message};
use tokio::sync::Notify;
use tracing::{debug, trace, warn};

use crate::{
    api::TorrentIdOrHash, peer_connection::WriterRequest, torrent_state::TorrentMetadata,
    type_aliases::PeerHandle,
};

use super::{TorrentStateLive, peer::PeerTx};

/// How many hashes of a piece layer one request asks for (libtorrent's `hash_picker` asks 512).
pub(crate) const HASH_REQUEST_CHUNK: u32 = 512;
/// Hash requests in flight to one peer at a time.
const MAX_REQUESTS_PER_PEER: usize = 4;
/// A request not answered by then goes to another peer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a chunk every peer turned down waits before they are asked again.
pub(crate) const RETRY_BACKOFF: Duration = Duration::from_secs(10);
/// Learned layers are written to the stored `.torrent` at most this often while some are
/// still missing (and right away once all are known).
const PERSIST_INTERVAL: Duration = Duration::from_secs(30);
/// How often the fetcher looks for timeouts and back-offs that ran out.
const FETCH_TICK: Duration = Duration::from_secs(1);

/// The trees of the files we answer for hold at most this many hashes in all (32 MiB). A tree
/// larger than that is built for each request instead (the answer budget bounds how often).
const MAX_CACHED_TREE_HASHES: usize = 1 << 20;

/// Answer budget of a peer: up to [`ANSWER_BURST`] requests at once, then
/// [`ANSWERS_PER_SECOND`].
pub(crate) const ANSWER_BURST: u32 = 64;
pub(crate) const ANSWERS_PER_SECOND: u32 = 16;
/// A peer whose requests were refused for being over its budget this many times is
/// disconnected.
pub(crate) const MAX_REFUSED_REQUESTS: u32 = 64;

/// A token bucket of the hash requests one peer may make.
#[derive(Debug)]
pub(crate) struct AnswerBudget {
    tokens: f64,
    refilled_at: Instant,
    refused: u32,
}

/// What to do with one request of a peer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BudgetVerdict {
    Answer,
    Refuse,
    Disconnect,
}

impl AnswerBudget {
    pub fn new(now: Instant) -> Self {
        Self {
            tokens: f64::from(ANSWER_BURST),
            refilled_at: now,
            refused: 0,
        }
    }

    pub fn take(&mut self, now: Instant) -> BudgetVerdict {
        let elapsed = now
            .saturating_duration_since(self.refilled_at)
            .as_secs_f64();
        self.refilled_at = now;
        self.tokens =
            (self.tokens + elapsed * f64::from(ANSWERS_PER_SECOND)).min(f64::from(ANSWER_BURST));
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            return BudgetVerdict::Answer;
        }
        self.refused += 1;
        if self.refused > MAX_REFUSED_REQUESTS {
            BudgetVerdict::Disconnect
        } else {
            BudgetVerdict::Refuse
        }
    }
}

#[derive(Default)]
struct TreeCache {
    // Most recently used last: (position in v2_files, tree, its hash count).
    trees: Vec<(usize, Arc<PieceLayerTree>, usize)>,
    hashes: usize,
}

impl TreeCache {
    fn get(&mut self, file: usize) -> Option<Arc<PieceLayerTree>> {
        let pos = self.trees.iter().position(|(f, ..)| *f == file)?;
        let entry = self.trees.remove(pos);
        let tree = entry.1.clone();
        self.trees.push(entry);
        Some(tree)
    }

    fn insert(&mut self, file: usize, tree: Arc<PieceLayerTree>, hashes: usize) {
        if hashes > MAX_CACHED_TREE_HASHES || self.trees.iter().any(|(f, ..)| *f == file) {
            return;
        }
        while self.hashes + hashes > MAX_CACHED_TREE_HASHES && !self.trees.is_empty() {
            let (_, _, n) = self.trees.remove(0);
            self.hashes -= n;
        }
        self.hashes += hashes;
        self.trees.push((file, tree, hashes));
    }
}

#[derive(Debug, Default)]
struct Chunk {
    done: bool,
    in_flight: Option<(PeerHandle, Instant)>,
    // Peers that rejected it, timed out or sent a forged answer.
    turned_down_by: HashSet<PeerHandle>,
    // Not asked again before this, once every peer turned it down.
    retry_at: Option<Instant>,
}

/// The fetch of one file's piece layer.
struct LayerFetch {
    // Position in v2_files.
    file: usize,
    pieces_root: Id32,
    file_len: u64,
    num_pieces: u32,
    // Layer of the requested hashes and the proof layers asked with them.
    base: u32,
    count: u32,
    proof_layers: u32,
    // The piece layer, padded to whole chunks.
    layer: Vec<Id32>,
    chunks: Vec<Chunk>,
}

impl LayerFetch {
    fn new(info: &ValidatedTorrentMetaV1Info<ByteBufOwned>, file: usize) -> Option<Self> {
        let f = *info.v2_files().get(file)?;
        let piece_length = info.lengths().default_piece_length();
        let base = merkle::piece_layer_height(piece_length)?;
        let tree_height = merkle::num_leafs(f.length)?.trailing_zeros();
        // The file is larger than a piece, so its piece layer is at least 2 wide.
        let layer_width = 1u64 << tree_height.checked_sub(base)?;
        let count = u32::try_from(layer_width.min(u64::from(HASH_REQUEST_CHUNK))).ok()?;
        let num_chunks = f.num_pieces.div_ceil(count) as usize;
        let proof_layers = tree_height - base - 1;
        Some(Self {
            file,
            pieces_root: f.pieces_root,
            file_len: f.length,
            num_pieces: f.num_pieces,
            base,
            count,
            proof_layers,
            layer: vec![Id32::default(); num_chunks * count as usize],
            chunks: (0..num_chunks).map(|_| Chunk::default()).collect(),
        })
    }

    // `chunk` is below `chunks.len()`, which is at most `num_pieces / count + 1`: the index
    // fits a u32.
    #[allow(clippy::cast_possible_truncation)]
    fn request(&self, chunk: usize) -> HashRequest {
        HashRequest {
            pieces_root: self.pieces_root,
            base_layer: self.base,
            index: chunk as u32 * self.count,
            length: self.count,
            proof_layers: self.proof_layers,
        }
    }

    // The chunk a request of ours is for.
    fn chunk_of(&self, r: &HashRequest) -> Option<usize> {
        if r.pieces_root != self.pieces_root
            || r.base_layer != self.base
            || r.length != self.count
            || r.proof_layers != self.proof_layers
            || !r.index.is_multiple_of(self.count)
        {
            return None;
        }
        let chunk = (r.index / self.count) as usize;
        (chunk < self.chunks.len()).then_some(chunk)
    }

    fn is_complete(&self) -> bool {
        self.chunks.iter().all(|c| c.done)
    }
}

#[derive(Default)]
struct FetchState {
    files: Vec<LayerFetch>,
    // Layers learned since the stored .torrent was last rewritten.
    unsaved: bool,
    saved_at: Option<Instant>,
}

/// The BEP 52 hash state of a live torrent.
pub(crate) struct HashState {
    trees: Mutex<TreeCache>,
    fetch: Mutex<FetchState>,
    fetcher_notify: Notify,
    // Hash requests peers sent us, answered or not.
    requests_received: AtomicU64,
    // Hash requests we sent.
    requests_sent: AtomicU64,
}

impl HashState {
    /// The fetches of every missing piece layer (one per pieces root).
    pub fn new(info: &ValidatedTorrentMetaV1Info<ByteBufOwned>) -> Self {
        let mut roots = HashSet::new();
        let files = info
            .missing_piece_layers()
            .filter(|f| {
                info.v2_files()
                    .get(*f)
                    .is_some_and(|f| roots.insert(f.pieces_root))
            })
            .filter_map(|f| LayerFetch::new(info, f))
            .collect();
        Self {
            trees: Default::default(),
            fetch: Mutex::new(FetchState {
                files,
                ..Default::default()
            }),
            fetcher_notify: Notify::new(),
            requests_received: Default::default(),
            requests_sent: Default::default(),
        }
    }

    /// True if some piece layer has to be fetched.
    pub fn is_fetching(&self) -> bool {
        !self.fetch.lock().files.is_empty()
    }

    /// Something changed for the fetcher: a v2 peer came, a layer was learned.
    pub fn wake_fetcher(&self) {
        self.fetcher_notify.notify_one();
    }
}

impl TorrentStateLive {
    /// Hash requests peers sent us, answered or not.
    #[cfg(test)]
    pub(crate) fn hash_requests_received(&self) -> u64 {
        self.hashes.requests_received.load(Ordering::Relaxed)
    }

    /// Hash requests we sent.
    #[cfg(test)]
    pub(crate) fn hash_requests_sent(&self) -> u64 {
        self.hashes.requests_sent.load(Ordering::Relaxed)
    }
}

impl TorrentStateLive {
    /// The answer to a hash request: the hashes, or None to reject it.
    pub(crate) fn answer_hash_request(&self, request: &HashRequest) -> Option<Vec<Id32>> {
        self.hashes
            .requests_received
            .fetch_add(1, Ordering::Relaxed);
        let info = &self.metadata.info;
        let file = info.v2_file_by_root(&request.pieces_root)?;
        let f = info.v2_files().get(file)?;
        let shape = request.shape();
        if !shape.is_valid_for(f.length) || shape.response_len() as usize > MAX_HASHES_PER_MESSAGE {
            return None;
        }
        let cached = self.hashes.trees.lock().get(file);
        let tree = match cached {
            Some(tree) => tree,
            None => {
                // Files of at most one piece have no piece layer: anything asked of them is
                // below it.
                let layer = info.piece_layer(file)?;
                let piece_length = info.lengths().default_piece_length();
                let tree = Arc::new(
                    self.shared
                        .spawner
                        .block_in_place(|| PieceLayerTree::new(layer, piece_length))?,
                );
                let hashes = layer.len().next_power_of_two() * 2;
                self.hashes.trees.lock().insert(file, tree.clone(), hashes);
                tree
            }
        };
        tree.hashes_for(&shape)
    }

    /// A `hashes` message from `peer`. Errors if it is a forged answer to one of our requests.
    pub(crate) fn on_received_hashes(
        &self,
        peer: PeerHandle,
        hashes: &Hashes<buffers::ByteBuf<'_>>,
    ) -> anyhow::Result<()> {
        let request = hashes.request;
        let received: Vec<Id32> = hashes.iter_hashes().collect();
        let completed = {
            let mut g = self.hashes.fetch.lock();
            let Some((pos, chunk)) = g
                .files
                .iter()
                .enumerate()
                .find_map(|(pos, f)| f.chunk_of(&request).map(|c| (pos, c)))
            else {
                trace!(?peer, ?request, "ignoring hashes we did not ask for");
                return Ok(());
            };
            let fetch = &mut g.files[pos];
            let c = &mut fetch.chunks[chunk];
            if c.done || c.in_flight.map(|(p, _)| p) != Some(peer) {
                trace!(
                    ?peer,
                    ?request,
                    "ignoring hashes we did not ask this peer for"
                );
                return Ok(());
            }
            c.in_flight = None;
            let shape = request.shape();
            if !merkle::verify_hashes(&fetch.pieces_root, fetch.file_len, &shape, &received) {
                c.turned_down_by.insert(peer);
                drop(g);
                self.hashes.wake_fetcher();
                bail!("peer sent hashes that do not match the pieces root: {request:?}");
            }
            c.done = true;
            let start = chunk * fetch.count as usize;
            fetch.layer[start..start + fetch.count as usize]
                .copy_from_slice(&received[..fetch.count as usize]);
            if fetch.is_complete() {
                Some(g.files.swap_remove(pos))
            } else {
                None
            }
        };
        if let Some(fetch) = completed {
            self.on_layer_fetched(fetch);
        }
        self.hashes.wake_fetcher();
        Ok(())
    }

    fn on_layer_fetched(&self, mut fetch: LayerFetch) {
        fetch.layer.truncate(fetch.num_pieces as usize);
        let res = self
            .shared
            .spawner
            .block_in_place(|| self.metadata.info.set_piece_layer(fetch.file, &fetch.layer));
        match res {
            Ok(_) => {
                debug!(file = fetch.file, "learned the piece layer of a file");
                self.hashes.fetch.lock().unsaved = true;
                // Its pieces can be downloaded now.
                self.new_pieces_notify.notify_waiters();
            }
            Err(e) => {
                // Every chunk was checked against the root, so this is a bug; start over.
                warn!(file = fetch.file, error = ?e, "fetched piece layer does not check");
                if let Some(fetch) = LayerFetch::new(&self.metadata.info, fetch.file) {
                    self.hashes.fetch.lock().files.push(fetch);
                }
            }
        }
    }

    /// A `hash reject` from `peer`: ask another peer.
    pub(crate) fn on_hash_reject(&self, peer: PeerHandle, request: &HashRequest) {
        let mut g = self.hashes.fetch.lock();
        let Some(c) = g
            .files
            .iter_mut()
            .find_map(|f| f.chunk_of(request).map(|c| &mut f.chunks[c]))
        else {
            return;
        };
        if c.in_flight.map(|(p, _)| p) == Some(peer) {
            debug!(?peer, ?request, "peer rejected a hash request");
            c.in_flight = None;
            c.turned_down_by.insert(peer);
            drop(g);
            self.hashes.wake_fetcher();
        }
    }

    // Sends the requests that can go out now. Returns the live v2 peers it looked at.
    fn schedule_hash_requests(&self, now: Instant) {
        let peers: Vec<(PeerHandle, PeerTx)> = self
            .peers
            .states
            .iter()
            .filter_map(|e| {
                e.value()
                    .get_live()
                    .filter(|l| l.v2)
                    .map(|l| (*e.key(), l.tx.clone()))
            })
            .collect();
        let live: HashSet<PeerHandle> = peers.iter().map(|(p, _)| *p).collect();

        let mut g = self.hashes.fetch.lock();
        let mut load: Vec<usize> = vec![0; peers.len()];
        // Requests to peers that left or did not answer in time are taken back.
        for c in g.files.iter_mut().flat_map(|f| f.chunks.iter_mut()) {
            let Some((peer, sent)) = c.in_flight else {
                continue;
            };
            if !live.contains(&peer) {
                c.in_flight = None;
            } else if now.saturating_duration_since(sent) >= REQUEST_TIMEOUT {
                debug!(?peer, "hash request timed out");
                c.in_flight = None;
                c.turned_down_by.insert(peer);
            } else if let Some(i) = peers.iter().position(|(p, _)| *p == peer) {
                load[i] += 1;
            }
        }
        if peers.is_empty() {
            return;
        }
        let mut sent = Vec::new();
        for f in g.files.iter_mut() {
            for chunk in 0..f.chunks.len() {
                let pick = {
                    let c = &f.chunks[chunk];
                    if c.done || c.in_flight.is_some() {
                        continue;
                    }
                    (0..peers.len())
                        .filter(|i| load[*i] < MAX_REQUESTS_PER_PEER)
                        .filter(|i| !c.turned_down_by.contains(&peers[*i].0))
                        .min_by_key(|i| load[*i])
                };
                let c = &mut f.chunks[chunk];
                let Some(i) = pick else {
                    // Every live peer turned it down: ask them again after a while.
                    let all_turned_down = peers.iter().all(|(p, _)| c.turned_down_by.contains(p));
                    if all_turned_down {
                        match c.retry_at {
                            None => c.retry_at = Some(now + RETRY_BACKOFF),
                            Some(t) if now >= t => {
                                c.turned_down_by.clear();
                                c.retry_at = None;
                            }
                            Some(_) => {}
                        }
                    }
                    continue;
                };
                load[i] += 1;
                c.in_flight = Some((peers[i].0, now));
                sent.push((i, f.request(chunk)));
            }
        }
        drop(g);
        for (i, request) in sent {
            trace!(peer = ?peers[i].0, ?request, "sending hash request");
            self.hashes.requests_sent.fetch_add(1, Ordering::Relaxed);
            // A closed channel means the peer is going away: the request is taken back on
            // the next tick.
            if peers[i]
                .1
                .send(WriterRequest::Message(Message::HashRequest(request)))
                .is_err()
            {
                trace!(peer = ?peers[i].0, "peer is gone, hash request not sent");
            }
        }
    }

    /// Fetches the missing piece layers, then writes them to the stored `.torrent`.
    pub(crate) async fn task_hash_fetcher(self: Arc<Self>) -> crate::Result<()> {
        loop {
            let now = Instant::now();
            self.schedule_hash_requests(now);
            let (missing, save) = {
                let g = self.hashes.fetch.lock();
                let missing = !g.files.is_empty();
                let save = g.unsaved
                    && (!missing
                        || g.saved_at
                            .is_none_or(|t| now.saturating_duration_since(t) >= PERSIST_INTERVAL));
                (missing, save)
            };
            if save {
                {
                    let mut g = self.hashes.fetch.lock();
                    g.unsaved = false;
                    g.saved_at = Some(now);
                }
                if let Err(e) = self.save_piece_layers().await {
                    warn!(error = ?e, "error storing the learned piece layers");
                }
            }
            if !missing && !self.hashes.fetch.lock().unsaved {
                return Ok(());
            }
            tokio::select! {
                _ = self.hashes.fetcher_notify.notified() => {}
                _ = tokio::time::sleep(FETCH_TICK) => {}
            }
        }
    }

    // Rewrites the torrent's `.torrent` bytes with the layers known now, and has the session
    // store them.
    async fn save_piece_layers(&self) -> anyhow::Result<()> {
        let session = self.shared.session.upgrade().context("session is dead")?;
        let handle = session
            .get(TorrentIdOrHash::Id(self.shared.id))
            .context("torrent is gone")?;
        let Some(current) = handle.metadata.load_full() else {
            return Ok(());
        };
        let layers = current.info.known_piece_layers();
        let torrent_bytes = self
            .shared
            .spawner
            .block_in_place(|| torrent_bytes_with_piece_layers(&current.torrent_bytes, &layers))?;
        handle.metadata.store(Some(Arc::new(TorrentMetadata {
            info: current.info.clone(),
            torrent_bytes,
            info_bytes: current.info_bytes.clone(),
            file_infos: current.file_infos.clone(),
        })));
        session.try_update_persistence_metadata(&handle).await;
        Ok(())
    }
}

/// The `.torrent` `torrent_bytes` with its `piece layers` replaced by `layers` (left out when
/// empty). Every other entry of the root dictionary, the info dictionary first of all, is kept
/// byte for byte, so the info hashes do not change.
pub(crate) fn torrent_bytes_with_piece_layers(
    torrent_bytes: &[u8],
    layers: &PieceLayers<ByteBufOwned>,
) -> anyhow::Result<Bytes> {
    use serde::Deserialize;

    const KEY: &[u8] = b"piece layers";
    let mut encoded_layers = Vec::new();
    if !layers.0.is_empty() {
        // A byte string: `&[u8]` itself would be serialized as a list of integers.
        bencode::bencode_serialize_to_writer(buffers::ByteBuf(KEY), &mut encoded_layers)
            .context("error serializing the piece layers key")?;
        bencode::bencode_serialize_to_writer(layers, &mut encoded_layers)
            .context("error serializing the piece layers")?;
    }
    let mut rest = torrent_bytes
        .strip_prefix(b"d")
        .context("torrent is not a dictionary")?;
    let mut out = Vec::with_capacity(torrent_bytes.len() + encoded_layers.len());
    out.push(b'd');
    let mut layers_written = false;
    loop {
        if let Some(after) = rest.strip_prefix(b"e") {
            if !after.is_empty() {
                bail!("trailing bytes after the torrent");
            }
            break;
        }
        let (key, after_key) = bencode::from_bytes_with_rest::<buffers::ByteBuf>(rest)
            .map_err(|e| e.into_anyhow())
            .context("error reading a torrent key")?;
        let mut de = bencode::BencodeDeserializer::new_from_buf(after_key);
        serde::de::IgnoredAny::deserialize(&mut de).context("error reading a torrent value")?;
        let after_value = de.into_remaining();
        let entry = &rest[..rest.len() - after_value.len()];
        // Keys are sorted: the layers go before the first key after theirs.
        if !layers_written && key.as_ref() > KEY {
            out.extend_from_slice(&encoded_layers);
            layers_written = true;
        }
        if key.as_ref() != KEY {
            out.extend_from_slice(entry);
        }
        rest = after_value;
    }
    if !layers_written {
        out.extend_from_slice(&encoded_layers);
    }
    out.push(b'e');
    Ok(out.into())
}

#[cfg(test)]
mod tests {
    use librqbit_core::{metainfo_v2::PieceLayers, torrent_metainfo::torrent_from_bytes};

    use super::torrent_bytes_with_piece_layers;
    use crate::tests::v2_torrent::{B, V2Files};

    /// Adding the layers to a torrent without them gives back the original bytes, keys before
    /// and after `piece layers` included; replacing them is a no-op; no layers removes them.
    #[test]
    fn test_torrent_bytes_with_piece_layers() {
        for hybrid in [false, true] {
            let files = V2Files::random(&[("a", 100_000), ("b/c", 40_000), ("d", 10)], 1);
            let mut full = files.torrent("t", 32 * 1024, hybrid);
            full.set("announce", B::s("http://example.com/announce"));
            full.set("url-list", B::s("http://example.com/files/"));
            let full_bytes = full.to_bytes();
            let mut stripped = full.clone();
            stripped.remove("piece layers");
            let stripped_bytes = stripped.to_bytes();

            let parsed = torrent_from_bytes(&full_bytes).unwrap();
            let layers = parsed
                .validate_with_piece_layers()
                .unwrap()
                .known_piece_layers();
            assert_eq!(layers.0.len(), 2);
            let rebuilt = torrent_bytes_with_piece_layers(&stripped_bytes, &layers).unwrap();
            assert_eq!(&rebuilt[..], &full_bytes[..], "hybrid={hybrid}");
            let replaced = torrent_bytes_with_piece_layers(&full_bytes, &layers).unwrap();
            assert_eq!(&replaced[..], &full_bytes[..], "hybrid={hybrid}");
            let removed =
                torrent_bytes_with_piece_layers(&full_bytes, &PieceLayers(Vec::new())).unwrap();
            assert_eq!(&removed[..], &stripped_bytes[..], "hybrid={hybrid}");

            assert!(torrent_bytes_with_piece_layers(b"le", &layers).is_err());
            let mut trailing = stripped_bytes.clone();
            trailing.push(b'x');
            assert!(torrent_bytes_with_piece_layers(&trailing, &layers).is_err());
        }
    }
}
