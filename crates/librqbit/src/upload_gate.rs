//! Bookkeeping for the upload requests of one peer connection.
//!
//! An accepted request waits in the upload rate limiter's queue, and then in the connection's
//! writer queue, before its block is read from disk and sent. Meanwhile the peer may cancel the
//! request (BEP 3 `cancel`), or we may choke the peer, which drops everything it asked for; the
//! block must then be neither read nor sent nor counted as uploaded. The gate remembers which
//! requests are still waiting and which of them were cancelled. It also carries the verdict of
//! the connection's [`PeerPolicy`](crate::PeerPolicy): a choked or disconnected connection drops
//! everything that waits.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering},
    },
};

use librqbit_core::lengths::ChunkInfo;
use parking_lot::Mutex;
use tokio::sync::mpsc::UnboundedSender;

use crate::{peer_connection::WriterRequest, peer_policy::PeerVerdict};

/// How many accepted upload requests may wait for one connection. Requests above it are
/// ignored; the peer asks again for what it didn't get. The limit is announced as `reqq`.
pub(crate) const MAX_QUEUED_UPLOADS_PER_PEER: u32 = 500;
/// The same for a throttled connection: the throttled queue is shared and slow, a few blocks per
/// connection keep its memory bounded and let throttled peers take turns.
pub(crate) const MAX_QUEUED_THROTTLED_UPLOADS_PER_PEER: u32 = 4;

/// (piece, begin, length): how requests and cancels identify a block.
pub(crate) type BlockKey = (u32, u32, u32);

pub(crate) fn block_key(chunk: &ChunkInfo) -> BlockKey {
    (chunk.piece_index.get(), chunk.offset, chunk.size)
}

#[derive(Default)]
struct PendingBlock {
    queued: u32,
    cancelled: u32,
    // An allowed-fast request (BEP 6): a choke does not drop it, it is served while the peer
    // is choked.
    exempt: bool,
}

#[derive(Default)]
struct Pending {
    blocks: HashMap<BlockKey, PendingBlock>,
    // Sums over `blocks`.
    queued: u32,
    cancelled: u32,
    // We unchoked the peer. A connection starts choked (BEP 3).
    unchoked: bool,
    // The allowed-fast set we offered the peer (BEP 6), and its blocks served while choked.
    // Under the same lock as the queue, so accepting a request past the choke is one decision.
    fast_upload: crate::fast::FastUpload,
}

/// Shared between the reader side of a connection (which registers requests and cancels), the
/// choker, the upload schedulers and the writer: it knows which accepted upload requests must
/// still be served. A request is dropped when the peer cancelled it, or when the connection
/// was choked (it lost its upload slot, or the policy choked or disconnected it). With the Fast
/// Extension the caller rejects what a choke dropped; allowed-fast requests are exempt and stay
/// queued.
#[derive(Default)]
pub(crate) struct UploadGate {
    level: AtomicU8,
    // Copies of the sums in `pending`, so that the common cases (nothing waits, nothing was
    // cancelled) are decided without taking the lock.
    queued: AtomicU32,
    cancelled: AtomicU32,
    pending: Mutex<Pending>,
    // Both handshakes carried the Fast Extension bit (BEP 6). Set once, at the handshake.
    fast: AtomicBool,
    // Rejects sent in the current choke period for requests turned down on arrival, see
    // `fast::MAX_REJECTS_PER_CHOKE`.
    rejects: AtomicU32,
}

/// What [`UploadGate::register_fast`] did with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Registered {
    /// Queued: the peer is unchoked.
    Queued,
    /// Queued past the choke, as an allowed-fast request.
    QueuedAllowedFast,
    /// Not queued: the peer is choked.
    Choked,
    /// Not queued: as many requests of the connection as allowed wait already.
    Full,
}

impl UploadGate {
    pub fn level(&self) -> PeerVerdict {
        PeerVerdict::from_u8(self.level.load(Ordering::Acquire))
    }

    /// Escalates the connection's verdict. Returns the previous one.
    pub fn raise(&self, to: PeerVerdict) -> PeerVerdict {
        PeerVerdict::from_u8(self.level.fetch_max(to as u8, Ordering::AcqRel))
    }

    /// Whether the choker (or the policy) currently allows this peer an upload slot.
    pub fn is_unchoked(&self) -> bool {
        self.pending.lock().unchoked
    }

    /// Records that the Fast Extension was negotiated on the connection.
    pub fn set_fast(&self, fast: bool) {
        self.fast.store(fast, Ordering::Release);
    }

    /// Whether the Fast Extension was negotiated on the connection: the peer then expects a
    /// `RejectRequest` for every request we will not serve.
    pub fn is_fast(&self) -> bool {
        self.fast.load(Ordering::Acquire)
    }

    /// Takes one reject out of the budget of the current choke period (see
    /// [`MAX_REJECTS_PER_CHOKE`](crate::fast::MAX_REJECTS_PER_CHOKE)). `false`: the peer keeps
    /// asking for what it is not given, the request is to be dropped without an answer.
    pub fn take_reject(&self) -> bool {
        self.rejects.fetch_add(1, Ordering::AcqRel) < crate::fast::MAX_REJECTS_PER_CHOKE
    }

    /// The peer asked for the block. `false`: the request is ignored, because the peer is
    /// choked or `max_queued` requests of the connection already wait.
    pub fn register(&self, key: BlockKey, max_queued: u32) -> bool {
        let mut g = self.pending.lock();
        if !g.unchoked || g.queued >= max_queued {
            return false;
        }
        g.blocks.entry(key).or_default().queued += 1;
        g.queued += 1;
        self.queued.store(g.queued, Ordering::Release);
        true
    }

    /// [`UploadGate::register`] for a connection with the Fast Extension (BEP 6), which needs
    /// to know why a request was not accepted to answer it. A block of a piece in the
    /// allowed-fast set we offered the peer is accepted while the peer is choked too, once per
    /// block and only while the policy has nothing against the peer (it bypasses the choke,
    /// not the queue bound), and a later choke does not drop it.
    pub fn register_fast(&self, key: BlockKey, max_queued: u32) -> Registered {
        let mut g = self.pending.lock();
        if g.queued >= max_queued {
            return Registered::Full;
        }
        let offered = g.fast_upload.is_offered(key.0);
        let unchoked = g.unchoked;
        if !unchoked
            && !(offered && self.level() == PeerVerdict::Allow && g.fast_upload.take_block(key))
        {
            return Registered::Choked;
        }
        let b = g.blocks.entry(key).or_default();
        b.queued += 1;
        b.exempt |= offered;
        g.queued += 1;
        self.queued.store(g.queued, Ordering::Release);
        if unchoked {
            Registered::Queued
        } else {
            Registered::QueuedAllowedFast
        }
    }

    /// Whether we already decided which allowed-fast set to offer the peer.
    pub fn allowed_fast_decided(&self) -> bool {
        self.pending.lock().fast_upload.decided()
    }

    /// Records the allowed-fast set offered to the peer (possibly empty); decided once.
    pub fn offer_allowed_fast(&self, pieces: Vec<u32>) {
        self.pending.lock().fast_upload.set_offered(pieces);
    }

    /// We choke or unchoke the peer. A choke drops every request that waits, except
    /// allowed-fast ones while the policy has nothing against the peer (a policy choke or
    /// disconnect drops them too). On a connection with the Fast Extension the dropped ones are
    /// returned (one entry per request still to be served) so the caller can reject each of
    /// them; a request the peer already cancelled is not returned, it was answered when the
    /// cancel arrived. Without the extension nothing is returned: the peer asks again once it is
    /// unchoked.
    pub fn set_choked(&self, choked: bool) -> Vec<BlockKey> {
        let mut g = self.pending.lock();
        g.unchoked = !choked;
        self.rejects.store(0, Ordering::Release);
        if !choked {
            return Vec::new();
        }
        let keep_exempt = self.level() == PeerVerdict::Allow;
        self.drop_blocks(&mut g, |b| !(keep_exempt && b.exempt))
    }

    fn drop_blocks(&self, g: &mut Pending, drop: impl Fn(&PendingBlock) -> bool) -> Vec<BlockKey> {
        // Only a Fast connection answers what was dropped; the others need no list.
        let collect = self.is_fast();
        let mut dropped = Vec::new();
        g.blocks.retain(|key, b| {
            if !drop(b) {
                return true;
            }
            if collect {
                let live = b.queued.saturating_sub(b.cancelled);
                dropped.extend(std::iter::repeat_n(*key, live as usize));
            }
            false
        });
        g.queued = g.blocks.values().map(|b| b.queued).sum();
        g.cancelled = g.blocks.values().map(|b| b.cancelled).sum();
        self.queued.store(g.queued, Ordering::Release);
        self.cancelled.store(g.cancelled, Ordering::Release);
        dropped
    }

    /// The peer cancelled the block. Only cancels a request that is still waiting: one that
    /// was already sent (or never made) has nothing to cancel, and remembering the cancel would
    /// swallow a later legitimate request for the same block. Returns whether a waiting request
    /// was cancelled, so a Fast connection can answer it with `RejectRequest`.
    pub fn cancel(&self, key: BlockKey) -> bool {
        if self.queued.load(Ordering::Acquire) == 0 {
            return false;
        }
        let mut g = self.pending.lock();
        if let Some(b) = g.blocks.get_mut(&key)
            && b.cancelled < b.queued
        {
            b.cancelled += 1;
            g.cancelled += 1;
            self.cancelled.store(g.cancelled, Ordering::Release);
            return true;
        }
        false
    }

    /// Takes one waiting request for the block out of the queue. Returns whether it has to be
    /// served: it does not when it was cancelled, or dropped by a choke.
    pub fn release(&self, key: BlockKey) -> bool {
        if self.queued.load(Ordering::Acquire) == 0 {
            return false;
        }
        let mut g = self.pending.lock();
        let Some(b) = g.blocks.get_mut(&key) else {
            return false;
        };
        b.queued -= 1;
        let cancelled = b.cancelled > 0;
        if cancelled {
            b.cancelled -= 1;
        }
        if b.queued == 0 {
            g.blocks.remove(&key);
        }
        g.queued -= 1;
        if cancelled {
            g.cancelled -= 1;
        }
        self.queued.store(g.queued, Ordering::Release);
        self.cancelled.store(g.cancelled, Ordering::Release);
        !cancelled
    }

    /// For the schedulers: `false` means the request is not to be served (and is released, if
    /// it was cancelled).
    pub fn keep(&self, chunk: &ChunkInfo) -> bool {
        let key = block_key(chunk);
        if self.queued.load(Ordering::Acquire) == 0 {
            return false;
        }
        let cancelled = match self.pending.lock().blocks.get(&key) {
            Some(b) => b.cancelled > 0,
            None => return false,
        };
        if cancelled {
            self.release(key);
        }
        !cancelled
    }

    /// For the writer, right before it reads the block from disk: takes the request out of the
    /// queue and tells whether it still has to be served.
    pub fn should_upload(&self, chunk: &ChunkInfo) -> bool {
        self.release(block_key(chunk))
    }
}

/// An accepted upload request on its way to the writer of the connection.
pub(crate) struct UploadRequest {
    pub tx: UnboundedSender<WriterRequest>,
    pub chunk: ChunkInfo,
    pub gate: Arc<UploadGate>,
}

#[cfg(test)]
mod tests {
    use librqbit_core::lengths::Lengths;

    use super::*;

    const CAP: u32 = MAX_QUEUED_UPLOADS_PER_PEER;

    fn unchoked() -> UploadGate {
        let g = UploadGate::default();
        g.set_choked(false);
        g
    }

    #[test]
    fn a_choked_peer_cannot_queue_requests() {
        let g = UploadGate::default();
        let k = (0, 0, 16384);
        assert!(!g.register(k, CAP), "a connection starts choked");
        assert!(!g.release(k));
        g.set_choked(false);
        assert!(g.register(k, CAP));
        assert!(g.release(k));
    }

    #[test]
    fn a_choke_drops_what_waits_and_a_block_asked_for_again_is_sent_once() {
        let lengths = Lengths::new(100_000, 32 * 1024).unwrap();
        let chunk = lengths
            .chunk_info_from_received_data(lengths.validate_piece_index(0).unwrap(), 0, 16384)
            .unwrap();
        let k = block_key(&chunk);
        let g = unchoked();
        assert!(g.register(k, CAP));
        assert!(g.register((1, 0, 16384), CAP));
        g.cancel((1, 0, 16384));
        g.set_choked(true);
        assert_eq!(g.queued.load(Ordering::Relaxed), 0);
        assert_eq!(g.cancelled.load(Ordering::Relaxed), 0);
        assert!(
            !g.keep(&chunk),
            "the scheduler skips what the choke dropped"
        );
        assert!(!g.register(k, CAP));

        // Unchoked again, the peer repeats the request while the old one is still on its way
        // to the writer: the block goes out for one of them only.
        g.set_choked(false);
        assert!(g.register(k, CAP));
        assert!(g.keep(&chunk));
        assert!(g.should_upload(&chunk));
        assert!(!g.should_upload(&chunk));
    }

    #[test]
    fn cancel_drops_exactly_the_cancelled_request() {
        let g = unchoked();
        let k = (3, 0, 16384);
        assert!(g.register(k, CAP));
        g.cancel(k);
        assert!(!g.release(k), "cancelled request must not be served");
        // The same block asked for again later is served.
        assert!(g.register(k, CAP));
        assert!(g.release(k));
    }

    #[test]
    fn cancel_without_pending_request_is_forgotten() {
        let g = unchoked();
        let k = (3, 0, 16384);
        g.cancel(k);
        g.register(k, CAP);
        assert!(
            g.release(k),
            "a stray cancel must not swallow a later request"
        );
        // Also when other requests are waiting.
        g.register((1, 0, 16384), CAP);
        g.cancel(k);
        g.register(k, CAP);
        assert!(g.release(k));
    }

    #[test]
    fn cancel_after_release_is_forgotten() {
        let g = unchoked();
        let k = (1, 16384, 16384);
        g.register(k, CAP);
        assert!(g.release(k));
        g.cancel(k);
        g.register(k, CAP);
        assert!(g.release(k));
    }

    #[test]
    fn duplicate_requests_are_cancelled_one_by_one() {
        let g = unchoked();
        let k = (0, 0, 16384);
        g.register(k, CAP);
        g.register(k, CAP);
        g.cancel(k);
        g.cancel(k);
        g.cancel(k); // more cancels than requests: capped
        assert!(!g.release(k));
        assert!(!g.release(k));
        g.register(k, CAP);
        assert!(g.release(k));
    }

    #[test]
    fn cancel_of_one_duplicate_keeps_the_other() {
        let g = unchoked();
        let k = (0, 0, 16384);
        g.register(k, CAP);
        g.register(k, CAP);
        g.cancel(k);
        assert!(!g.release(k));
        assert!(g.release(k));
    }

    #[test]
    fn cancel_does_not_touch_other_blocks() {
        let g = unchoked();
        g.register((0, 0, 16384), CAP);
        g.register((0, 16384, 16384), CAP);
        g.cancel((0, 0, 16384));
        assert!(g.release((0, 16384, 16384)));
        assert!(!g.release((0, 0, 16384)));
    }

    #[test]
    fn requests_waiting_per_connection_are_bounded() {
        let g = unchoked();
        for i in 0..3 {
            assert!(g.register((i, 0, 16384), 3));
        }
        assert!(!g.register((9, 0, 16384), 3), "over the limit: ignored");
        assert!(!g.register((0, 0, 16384), 3), "also for a duplicate");
        // The ignored ones left no trace.
        assert!(g.release((0, 0, 16384)));
        assert!(g.register((9, 0, 16384), 3), "a slot is free again");
        assert!(g.release((9, 0, 16384)));
        assert!(g.release((1, 0, 16384)));
        assert!(g.release((2, 0, 16384)));
        assert_eq!(g.queued.load(Ordering::Relaxed), 0);
        assert_eq!(g.cancelled.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn counts_stay_in_step_with_cancels_and_releases() {
        let g = unchoked();
        g.register((0, 0, 16384), CAP);
        g.register((1, 0, 16384), CAP);
        g.cancel((1, 0, 16384));
        assert_eq!(g.queued.load(Ordering::Relaxed), 2);
        assert_eq!(g.cancelled.load(Ordering::Relaxed), 1);
        assert!(!g.release((1, 0, 16384)));
        assert_eq!(g.cancelled.load(Ordering::Relaxed), 0);
        assert!(g.release((0, 0, 16384)));
        assert_eq!(g.queued.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn keep_drops_and_releases_cancelled_requests() {
        let lengths = Lengths::new(100_000, 32 * 1024).unwrap();
        let chunk = lengths
            .chunk_info_from_received_data(lengths.validate_piece_index(0).unwrap(), 0, 16384)
            .unwrap();
        let g = unchoked();
        g.register(block_key(&chunk), CAP);
        assert!(g.keep(&chunk));
        g.cancel(block_key(&chunk));
        assert!(!g.keep(&chunk));
        assert_eq!(g.queued.load(Ordering::Relaxed), 0);
        // Nothing left for the writer to trip over.
        g.register(block_key(&chunk), CAP);
        assert!(g.should_upload(&chunk));
    }
}
