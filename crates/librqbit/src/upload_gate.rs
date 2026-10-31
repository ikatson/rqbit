//! Bookkeeping for the upload requests of one peer connection.
//!
//! An accepted request waits in the upload rate limiter's queue, and then in the connection's
//! writer queue, before its block is read from disk and sent. A peer may cancel the request
//! meanwhile (BEP 3 `cancel`); the block must then be neither read nor sent nor counted as
//! uploaded. The gate remembers which requests are still waiting and which of them were
//! cancelled. It also carries the verdict of the connection's
//! [`PeerPolicy`](crate::PeerPolicy): a choked or disconnected connection drops everything that
//! waits.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU32, Ordering},
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
}

#[derive(Default)]
struct Pending {
    blocks: HashMap<BlockKey, PendingBlock>,
    // Sums over `blocks`.
    queued: u32,
    cancelled: u32,
}

/// Shared between the reader side of a connection (which registers requests and cancels), the
/// upload schedulers and the writer.
#[derive(Default)]
pub(crate) struct UploadGate {
    level: AtomicU8,
    // Copies of the sums in `pending`, so that the common case (nothing cancelled, nothing
    // choked) is decided without taking the lock.
    queued: AtomicU32,
    cancelled: AtomicU32,
    pending: Mutex<Pending>,
}

impl UploadGate {
    pub fn level(&self) -> PeerVerdict {
        PeerVerdict::from_u8(self.level.load(Ordering::Acquire))
    }

    /// Escalates the connection's verdict. Returns the previous one.
    pub fn raise(&self, to: PeerVerdict) -> PeerVerdict {
        PeerVerdict::from_u8(self.level.fetch_max(to as u8, Ordering::AcqRel))
    }

    /// A request for the block was accepted for upload, unless `max_queued` requests of the
    /// connection already wait. Returns whether it was.
    pub fn register(&self, key: BlockKey, max_queued: u32) -> bool {
        let mut g = self.pending.lock();
        if g.queued >= max_queued {
            return false;
        }
        g.blocks.entry(key).or_default().queued += 1;
        g.queued += 1;
        self.queued.store(g.queued, Ordering::Release);
        true
    }

    /// The peer cancelled the block. Only cancels a request that is still waiting: one that
    /// was already sent (or never made) has nothing to cancel, and remembering the cancel would
    /// swallow a later legitimate request for the same block.
    pub fn cancel(&self, key: BlockKey) {
        if self.queued.load(Ordering::Acquire) == 0 {
            return;
        }
        let mut g = self.pending.lock();
        if let Some(b) = g.blocks.get_mut(&key)
            && b.cancelled < b.queued
        {
            b.cancelled += 1;
            g.cancelled += 1;
            self.cancelled.store(g.cancelled, Ordering::Release);
        }
    }

    /// Forgets everything that waits (the connection was choked or is going away).
    pub fn drop_queued(&self) {
        let mut g = self.pending.lock();
        *g = Pending::default();
        self.queued.store(0, Ordering::Release);
        self.cancelled.store(0, Ordering::Release);
    }

    fn is_dropped(&self, key: &BlockKey) -> bool {
        self.level() >= PeerVerdict::Choke
            || (self.cancelled.load(Ordering::Acquire) > 0
                && self
                    .pending
                    .lock()
                    .blocks
                    .get(key)
                    .is_some_and(|b| b.cancelled > 0))
    }

    /// Takes one waiting request for the block out of the queue. Returns whether it has to be
    /// served.
    pub fn release(&self, key: BlockKey) -> bool {
        if self.queued.load(Ordering::Acquire) == 0 {
            // Nothing waits (the queue was dropped by a choke, or the request never was one).
            return self.level() < PeerVerdict::Choke;
        }
        let mut g = self.pending.lock();
        let mut cancelled = false;
        if let Some(b) = g.blocks.get_mut(&key) {
            b.queued -= 1;
            if b.cancelled > 0 {
                b.cancelled -= 1;
                cancelled = true;
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
        }
        drop(g);
        !cancelled && self.level() < PeerVerdict::Choke
    }

    /// For the schedulers: `false` means the request was dropped (and is released).
    pub fn keep(&self, chunk: &ChunkInfo) -> bool {
        let key = block_key(chunk);
        if self.is_dropped(&key) {
            self.release(key);
            return false;
        }
        true
    }

    /// For the writer, right before it reads the block from disk: takes the request out of the
    /// queue and tells whether it was dropped meanwhile.
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

    #[test]
    fn cancel_drops_exactly_the_cancelled_request() {
        let g = UploadGate::default();
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
        let g = UploadGate::default();
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
        let g = UploadGate::default();
        let k = (1, 16384, 16384);
        g.register(k, CAP);
        assert!(g.release(k));
        g.cancel(k);
        g.register(k, CAP);
        assert!(g.release(k));
    }

    #[test]
    fn duplicate_requests_are_cancelled_one_by_one() {
        let g = UploadGate::default();
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
        let g = UploadGate::default();
        let k = (0, 0, 16384);
        g.register(k, CAP);
        g.register(k, CAP);
        g.cancel(k);
        assert!(!g.release(k));
        assert!(g.release(k));
    }

    #[test]
    fn cancel_does_not_touch_other_blocks() {
        let g = UploadGate::default();
        g.register((0, 0, 16384), CAP);
        g.register((0, 16384, 16384), CAP);
        g.cancel((0, 0, 16384));
        assert!(g.release((0, 16384, 16384)));
        assert!(!g.release((0, 0, 16384)));
    }

    #[test]
    fn requests_waiting_per_connection_are_bounded() {
        let g = UploadGate::default();
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
        let g = UploadGate::default();
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
    fn choke_drops_everything_queued_and_everything_later() {
        let g = UploadGate::default();
        g.register((0, 0, 16384), CAP);
        g.register((1, 0, 16384), CAP);
        g.raise(PeerVerdict::Choke);
        g.drop_queued();
        assert!(!g.release((0, 0, 16384)));
        assert!(!g.release((1, 0, 16384)));
        // A request that raced the choke is dropped as well.
        g.register((2, 0, 16384), CAP);
        assert!(!g.release((2, 0, 16384)));
        assert_eq!(g.queued.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn keep_drops_and_releases_dropped_requests() {
        let lengths = Lengths::new(100_000, 32 * 1024).unwrap();
        let chunk = lengths
            .chunk_info_from_received_data(lengths.validate_piece_index(0).unwrap(), 0, 16384)
            .unwrap();
        let g = UploadGate::default();
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
