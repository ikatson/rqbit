//! Super seeding (BEP 16, "initial seeding").
//!
//! A seeder that is the only source of a torrent uploads the same pieces many times over when
//! its peers all download from it. In super seeding mode it pretends to have nothing (no
//! bitfield, or `HaveNone` with the Fast Extension) and shows each connection one piece at a
//! time with a `Have`: the one the fewest connected peers have, preferring pieces that are not
//! being shown to another connection, ties broken at random. The next piece is shown once
//! - the connection has the previous one: we uploaded every block of it, or the peer announced
//!   it; and
//! - the piece showed up at some other peer: another connection announced `Have` for it, which
//!   proves the peer passes on what it gets (libtorrent's rule).
//!
//! The second condition is waived when nobody else could take the piece (every other
//! connected peer has it, or there is no other peer), and after [`REVEAL_TIMEOUT`], so that a
//! swarm of one, or of peers that do not trade, does not stall.
//!
//! Only the pieces shown to a connection are served to it. Requests for others are rejected
//! (Fast) or ignored, and the connection stays up.
//!
//! The mode of a connection is decided when it is made: on when the flag is on, the torrent has
//! every selected piece and uploads are allowed. Connections made before that keep seeing
//! everything. It ends for a connection when the flag is turned off, the selection is no longer
//! complete or the peer is a seed. The connection is then told about the pieces it was not
//! shown, with one `Have` each, and goes on as a normal one. Once every piece has been seen in
//! the swarm the mode keeps restricting, as libtorrent does, until the flag is turned off.
//!
//! The per-connection state lives with the connection and starts over with it, also when the
//! live state is rebuilt after a pause and resume. The flag lives with the torrent's options.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use librqbit_core::lengths::{ChunkInfo, Lengths, ValidPieceIndex};
use parking_lot::Mutex;
use peer_binary_protocol::Message;
use rand::RngExt;
use tokio::sync::Notify;
use tracing::{debug, trace};

use crate::{
    Error,
    peer_connection::WriterRequest,
    type_aliases::{BF, PeerHandle},
};

use super::{PeerHandler, TorrentStateLive, make_piece_bitfield};

/// How long a connection that has its piece waits for the piece to show up at another peer
/// before it is shown the next one anyway. libtorrent waits without bound, so a peer that does
/// not share stays on its piece forever; we bound it by the choker's optimistic unchoke interval,
/// the time a peer that downloads from nobody else waits for its next chance anyway.
pub(crate) const REVEAL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for the piece set of a peer before it is shown its first piece. A peer that
/// has nothing may send no piece set at all, and nothing else before it is shown something.
pub(crate) const PIECE_SET_WAIT: Duration = Duration::from_secs(2);

/// How many catch-up `Have`s are queued to the writer at once when the mode ends for a
/// connection, before the writer gets to send them.
const HAVE_BATCH: usize = 256;

#[allow(clippy::cast_possible_truncation)]
fn millis(d: Duration) -> u64 {
    d.as_millis().min(u128::from(u64::MAX)) as u64
}

/// Torrent-wide part of super seeding: which piece each super seeding connection is shown.
pub(crate) struct SuperSeeding {
    // A leaf lock: nothing else is locked while it is held.
    shown: Mutex<Shown>,
    reveal_timeout_ms: AtomicU64,
    piece_set_wait_ms: AtomicU64,
}

impl Default for SuperSeeding {
    fn default() -> Self {
        Self {
            shown: Default::default(),
            reveal_timeout_ms: AtomicU64::new(millis(REVEAL_TIMEOUT)),
            piece_set_wait_ms: AtomicU64::new(millis(PIECE_SET_WAIT)),
        }
    }
}

#[derive(Default)]
struct Shown {
    next_id: u64,
    conns: HashMap<PeerHandle, ShownTo>,
}

struct ShownTo {
    // Tells this connection apart from an older or newer one from the same address.
    id: u64,
    piece: Option<ValidPieceIndex>,
    // Another connection announced `Have` for `piece` since it was shown.
    spread: bool,
    notify: Arc<Notify>,
}

impl SuperSeeding {
    /// Wakes every super seeding connection to look at its state again: the flag or the
    /// selection changed, or a peer left.
    pub(crate) fn wake_all(&self) {
        for c in self.shown.lock().conns.values() {
            c.notify.notify_one();
        }
    }

    /// Changes the bounded waits, so that tests don't take seconds.
    #[cfg(test)]
    pub(crate) fn set_timeouts(&self, reveal: Duration, piece_set: Duration) {
        self.reveal_timeout_ms
            .store(millis(reveal), Ordering::Relaxed);
        self.piece_set_wait_ms
            .store(millis(piece_set), Ordering::Relaxed);
    }

    fn reveal_timeout(&self) -> Duration {
        Duration::from_millis(self.reveal_timeout_ms.load(Ordering::Relaxed))
    }

    fn piece_set_wait(&self) -> Duration {
        Duration::from_millis(self.piece_set_wait_ms.load(Ordering::Relaxed))
    }

    fn register(&self, addr: PeerHandle, notify: Arc<Notify>) -> u64 {
        let mut g = self.shown.lock();
        g.next_id += 1;
        let id = g.next_id;
        g.conns.insert(
            addr,
            ShownTo {
                id,
                piece: None,
                spread: false,
                notify,
            },
        );
        id
    }

    fn unregister(&self, addr: PeerHandle, id: u64) {
        let mut g = self.shown.lock();
        if g.conns.get(&addr).is_some_and(|c| c.id == id) {
            g.conns.remove(&addr);
        }
    }

    fn show(&self, addr: PeerHandle, id: u64, piece: ValidPieceIndex) {
        if let Some(c) = self.shown.lock().conns.get_mut(&addr)
            && c.id == id
        {
            c.piece = Some(piece);
            c.spread = false;
        }
    }

    fn is_spread(&self, addr: PeerHandle, id: u64) -> bool {
        self.shown
            .lock()
            .conns
            .get(&addr)
            .is_some_and(|c| c.id == id && c.spread)
    }

    /// The peer at `from` announced `piece`: it spread from every other connection it is
    /// shown to.
    pub(super) fn on_have(&self, from: PeerHandle, piece: ValidPieceIndex) {
        for (addr, c) in self.shown.lock().conns.iter_mut() {
            if *addr != from && c.piece == Some(piece) && !c.spread {
                c.spread = true;
                c.notify.notify_one();
            }
        }
    }

    /// The pieces shown to the other connections, sorted.
    fn shown_to_others(&self, addr: PeerHandle) -> Vec<u32> {
        let mut pieces: Vec<u32> = self
            .shown
            .lock()
            .conns
            .iter()
            .filter(|(a, _)| **a != addr)
            .filter_map(|(_, c)| c.piece.map(|p| p.get()))
            .collect();
        pieces.sort_unstable();
        pieces
    }
}

/// One connection's part of super seeding.
pub(super) struct SuperSeedConn {
    shared: Arc<SuperSeeding>,
    addr: PeerHandle,
    // 0 when the mode was never on for the connection.
    id: u64,
    // Set when the connection is made, cleared once when the mode ends for it.
    active: AtomicBool,
    notify: Arc<Notify>,
    // A leaf lock: nothing else is locked while it is held.
    reveal: Mutex<Reveal>,
}

#[derive(Default)]
struct Reveal {
    // Every piece shown to the connection so far.
    revealed: BF,
    current: Option<Current>,
}

struct Current {
    piece: ValidPieceIndex,
    chunks: usize,
    uploaded: BF,
    // When the peer got the whole piece.
    done_at: Option<Instant>,
}

impl SuperSeedConn {
    pub(super) fn new(
        shared: &Arc<SuperSeeding>,
        addr: PeerHandle,
        active: bool,
        lengths: &Lengths,
    ) -> Self {
        let notify = Arc::new(Notify::new());
        let (id, revealed) = if active {
            (
                shared.register(addr, notify.clone()),
                make_piece_bitfield(lengths),
            )
        } else {
            (0, BF::default())
        };
        Self {
            shared: shared.clone(),
            addr,
            id,
            active: AtomicBool::new(active),
            notify,
            reveal: Mutex::new(Reveal {
                revealed,
                current: None,
            }),
        }
    }

    pub(super) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// The peer may not be served `piece`: it was not shown it.
    pub(super) fn hides(&self, piece: ValidPieceIndex) -> bool {
        self.is_active()
            && !self
                .reveal
                .lock()
                .revealed
                .get(piece.get_usize())
                .is_some_and(|b| *b)
    }

    fn current(&self) -> Option<(ValidPieceIndex, Option<Instant>)> {
        self.reveal
            .lock()
            .current
            .as_ref()
            .map(|c| (c.piece, c.done_at))
    }

    pub(super) fn current_piece(&self) -> Option<ValidPieceIndex> {
        if !self.is_active() {
            return None;
        }
        self.current().map(|(p, _)| p)
    }

    pub(super) fn wake(&self) {
        if self.is_active() {
            self.notify.notify_one();
        }
    }

    fn set_current(&self, piece: ValidPieceIndex, chunks: u32) {
        let chunks = chunks as usize;
        let mut g = self.reveal.lock();
        if let Some(mut bit) = g.revealed.get_mut(piece.get_usize()) {
            *bit = true;
        }
        g.current = Some(Current {
            piece,
            chunks,
            uploaded: BF::from_boxed_slice(vec![0; chunks.div_ceil(8)].into_boxed_slice()),
            done_at: None,
        });
    }

    /// A block of `chunk` went out to the peer.
    pub(super) fn on_uploaded(&self, chunk: &ChunkInfo) {
        if !self.is_active() {
            return;
        }
        let mut g = self.reveal.lock();
        let Some(c) = g.current.as_mut() else {
            return;
        };
        if c.piece != chunk.piece_index || c.done_at.is_some() {
            return;
        }
        if let Some(mut bit) = c.uploaded.get_mut(chunk.chunk_index as usize) {
            *bit = true;
        }
        if c.uploaded.get(..c.chunks).is_some_and(|b| b.all()) {
            c.done_at = Some(Instant::now());
            self.notify.notify_one();
        }
    }

    /// The peer has `piece` now (it announced it). Wakes the connection, which may be done
    /// with its piece or with the mode.
    pub(super) fn on_peer_has(&self, piece: ValidPieceIndex) {
        if !self.is_active() {
            return;
        }
        if let Some(c) = self.reveal.lock().current.as_mut()
            && c.piece == piece
            && c.done_at.is_none()
        {
            c.done_at = Some(Instant::now());
        }
        self.notify.notify_one();
    }

    /// Ends the mode for the connection. Returns the pieces it was shown.
    fn deactivate(&self) -> BF {
        self.active.store(false, Ordering::Release);
        self.shared.unregister(self.addr, self.id);
        let mut g = self.reveal.lock();
        g.current = None;
        std::mem::take(&mut g.revealed)
    }
}

impl Drop for SuperSeedConn {
    fn drop(&mut self) {
        if self.id != 0 {
            self.shared.unregister(self.addr, self.id);
            // Others may have waited for this peer to take their piece.
            self.shared.wake_all();
        }
    }
}

impl TorrentStateLive {
    /// The super seeding part of a new connection, with the mode on if it applies now.
    pub(super) fn super_seed_conn(&self, addr: PeerHandle) -> SuperSeedConn {
        let active = self.shared.options.super_seeding.load(Ordering::Acquire)
            && self.selection_complete.load(Ordering::Acquire)
            && !self.shared.options.disable_upload();
        SuperSeedConn::new(&self.super_seeding, addr, active, &self.lengths)
    }
}

impl PeerHandler {
    fn peer_is_seed(&self) -> bool {
        self.state
            .peers
            .with_live(self.addr, |l| l.seeder)
            .unwrap_or_default()
    }

    fn super_seeding_goes_on(&self) -> bool {
        self.state
            .shared
            .options
            .super_seeding
            .load(Ordering::Acquire)
            && self.state.selection_complete.load(Ordering::Acquire)
            && !self.peer_is_seed()
    }

    /// Some connected peer other than this one lacks `piece`.
    fn someone_else_lacks(&self, piece: ValidPieceIndex) -> bool {
        self.state.peers.states.iter().any(|e| {
            *e.key() != self.addr
                && e.value()
                    .get_live()
                    .is_some_and(|l| !l.bitfield.get(piece.get_usize()).is_some_and(|b| *b))
        })
    }

    /// While the mode is on for the connection, shows the peer one piece at a time. When it
    /// ends, tells the peer about everything else we have and returns, for the requester to
    /// go on as for any connection.
    pub(super) async fn run_super_seeding(&self) -> crate::Result<()> {
        if !self.super_seed.is_active() {
            return Ok(());
        }
        let shared = self.state.super_seeding.clone();
        let piece_set = self.wait_for_any_notify(&self.on_bitfield_notify, || {
            self.first_message_received.load(Ordering::Relaxed)
                || self
                    .state
                    .peers
                    .with_live(self.addr, |l| !l.bitfield.is_empty())
                    .unwrap_or_default()
        });
        if tokio::time::timeout(shared.piece_set_wait(), piece_set)
            .await
            .is_err()
        {
            trace!("super seeding: no piece set from the peer, assuming it has nothing");
        }

        loop {
            let notified = self.super_seed.notify.notified();
            if !self.super_seeding_goes_on() {
                break;
            }
            match self.super_seed.current() {
                None => {}
                Some((_, None)) => {
                    notified.await;
                    continue;
                }
                Some((piece, Some(done_at))) => {
                    let waited = done_at.elapsed();
                    let timeout = shared.reveal_timeout();
                    let next = shared.is_spread(self.addr, self.super_seed.id)
                        || waited >= timeout
                        || !self.someone_else_lacks(piece);
                    if !next {
                        if tokio::time::timeout(timeout - waited, notified)
                            .await
                            .is_err()
                        {
                            trace!(piece = piece.get(), "super seeding: piece did not spread");
                        }
                        continue;
                    }
                }
            }
            match self.pick_super_seed_piece()? {
                Some(piece) => self.reveal_piece(piece)?,
                // Nothing we have is left for the peer: wait for the flag or the selection
                // to change.
                None => notified.await,
            }
        }
        self.end_super_seeding().await
    }

    /// The piece to show the peer next: one we have and it neither has nor was shown, the
    /// fewest connected peers have, preferably not shown to another connection.
    fn pick_super_seed_piece(&self) -> crate::Result<Option<ValidPieceIndex>> {
        let peer_has = self
            .state
            .peers
            .with_live(self.addr, |l| l.bitfield.clone())
            .unwrap_or_default();
        let others = self.state.super_seeding.shown_to_others(self.addr);
        let total = self.state.lengths.total_pieces() as usize;
        let is_set = |bits: &BF, p: usize| bits.get(p).is_some_and(|b| *b);

        let g = self.state.lock_read("pick_super_seed_piece");
        let have = g.get_chunks()?.get_have_pieces().as_slice();
        // The innermost lock.
        let reveal = self.super_seed.reveal.lock();
        let mut rng = rand::rng();
        let mut best: Option<(bool, u32)> = None;
        let mut ties = 0u32;
        let mut chosen = None;
        for p in have.iter_ones().take_while(|p| *p < total) {
            if is_set(&peer_has, p) || is_set(&reveal.revealed, p) {
                continue;
            }
            let Ok(index) = u32::try_from(p) else {
                break;
            };
            let key = (
                others.binary_search(&index).is_ok(),
                self.state.peers.availability.get(p),
            );
            match best {
                Some(b) if key > b => {}
                Some(b) if key == b => {
                    ties += 1;
                    if rng.random_range(0..ties) == 0 {
                        chosen = Some(index);
                    }
                }
                _ => {
                    best = Some(key);
                    ties = 1;
                    chosen = Some(index);
                }
            }
        }
        Ok(chosen.and_then(|p| self.state.lengths.validate_piece_index(p)))
    }

    fn reveal_piece(&self, piece: ValidPieceIndex) -> crate::Result<()> {
        self.super_seed
            .set_current(piece, self.state.lengths.chunks_per_piece(piece));
        self.state
            .super_seeding
            .show(self.addr, self.super_seed.id, piece);
        trace!(piece = piece.get(), "super seeding: revealing");
        self.tx
            .send(WriterRequest::Message(Message::Have(piece.get())))
            .ok()
            .ok_or(Error::PeerTaskDead)?;
        Ok(())
    }

    /// Ends the mode for the connection and sends a `Have` for every piece we have that it was
    /// not shown: at most one per piece of the torrent, queued in batches so that the writer
    /// sends them as they come.
    async fn end_super_seeding(&self) -> crate::Result<()> {
        // First, so that from here on new pieces are announced and requests served as usual.
        let revealed = self.super_seed.deactivate();
        if self.peer_is_seed() {
            debug!("super seeding ended for the connection: the peer is a seed");
            return Ok(());
        }
        let mut missing: BF = {
            let g = self.state.lock_read("end_super_seeding");
            g.get_chunks()?
                .get_have_pieces()
                .as_slice()
                .to_bitvec()
                .into_boxed_bitslice()
        };
        for p in revealed.iter_ones() {
            if let Some(mut bit) = missing.get_mut(p) {
                *bit = false;
            }
        }
        let total = self.state.lengths.total_pieces() as usize;
        let mut sent = 0usize;
        for p in missing.iter_ones().take_while(|p| *p < total) {
            let Ok(index) = u32::try_from(p) else {
                break;
            };
            self.tx
                .send(WriterRequest::Message(Message::Have(index)))
                .ok()
                .ok_or(Error::PeerTaskDead)?;
            sent += 1;
            if sent % HAVE_BATCH == 0 {
                tokio::task::yield_now().await;
            }
        }
        debug!(
            sent,
            "super seeding ended for the connection, announced the pieces it was not shown"
        );
        Ok(())
    }
}
