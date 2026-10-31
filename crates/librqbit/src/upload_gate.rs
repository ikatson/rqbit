//! Bookkeeping for the upload requests of one peer connection.
//!
//! An accepted request waits in the upload rate limiter's queue, and then in the connection's
//! writer queue, before its block is read from disk and sent. A peer may cancel the request
//! meanwhile (BEP 3 `cancel`); the block must then be neither read nor sent nor counted as
//! uploaded. The gate remembers which requests are still waiting and which of them were
//! cancelled.

use std::{
    collections::HashMap,
    sync::atomic::{AtomicU32, Ordering},
};

use librqbit_core::lengths::ChunkInfo;
use parking_lot::Mutex;
use tokio::sync::mpsc::UnboundedSender;

use crate::peer_connection::WriterRequest;

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
/// upload scheduler and the writer.
#[derive(Default)]
pub(crate) struct UploadGate {
    // Copies of the sums in `pending`, so that the common case (nothing cancelled) is decided
    // without taking the lock.
    queued: AtomicU32,
    cancelled: AtomicU32,
    pending: Mutex<Pending>,
}

impl UploadGate {
    /// A request for the block was accepted for upload.
    pub fn register(&self, key: BlockKey) {
        let mut g = self.pending.lock();
        g.blocks.entry(key).or_default().queued += 1;
        g.queued += 1;
        self.queued.store(g.queued, Ordering::Release);
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

    fn is_cancelled(&self, key: &BlockKey) -> bool {
        self.cancelled.load(Ordering::Acquire) > 0
            && self
                .pending
                .lock()
                .blocks
                .get(key)
                .is_some_and(|b| b.cancelled > 0)
    }

    /// Takes one waiting request for the block out of the queue. Returns whether it has to be
    /// served.
    fn release(&self, key: BlockKey) -> bool {
        if self.queued.load(Ordering::Acquire) == 0 {
            return true;
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
        !cancelled
    }

    /// For the scheduler: `false` means the request was cancelled (and is released).
    pub fn keep(&self, chunk: &ChunkInfo) -> bool {
        let key = block_key(chunk);
        if self.is_cancelled(&key) {
            self.release(key);
            return false;
        }
        true
    }

    /// For the writer, right before it reads the block from disk: takes the request out of the
    /// queue and tells whether it was cancelled meanwhile.
    pub fn should_upload(&self, chunk: &ChunkInfo) -> bool {
        self.release(block_key(chunk))
    }
}

/// An accepted upload request on its way to the writer of the connection.
pub(crate) struct UploadRequest {
    pub tx: UnboundedSender<WriterRequest>,
    pub chunk: ChunkInfo,
    pub gate: std::sync::Arc<UploadGate>,
}

#[cfg(test)]
mod tests {
    use librqbit_core::lengths::Lengths;

    use super::*;

    #[test]
    fn cancel_drops_exactly_the_cancelled_request() {
        let g = UploadGate::default();
        let k = (3, 0, 16384);
        g.register(k);
        g.cancel(k);
        assert!(!g.release(k), "cancelled request must not be served");
        // The same block asked for again later is served.
        g.register(k);
        assert!(g.release(k));
    }

    #[test]
    fn cancel_without_pending_request_is_forgotten() {
        let g = UploadGate::default();
        let k = (3, 0, 16384);
        g.cancel(k);
        g.register(k);
        assert!(
            g.release(k),
            "a stray cancel must not swallow a later request"
        );
        // Also when other requests are waiting.
        g.register((1, 0, 16384));
        g.cancel(k);
        g.register(k);
        assert!(g.release(k));
    }

    #[test]
    fn cancel_after_release_is_forgotten() {
        let g = UploadGate::default();
        let k = (1, 16384, 16384);
        g.register(k);
        assert!(g.release(k));
        g.cancel(k);
        g.register(k);
        assert!(g.release(k));
    }

    #[test]
    fn duplicate_requests_are_cancelled_one_by_one() {
        let g = UploadGate::default();
        let k = (0, 0, 16384);
        g.register(k);
        g.register(k);
        g.cancel(k);
        g.cancel(k);
        g.cancel(k); // more cancels than requests: capped
        assert!(!g.release(k));
        assert!(!g.release(k));
        g.register(k);
        assert!(g.release(k));
    }

    #[test]
    fn cancel_of_one_duplicate_keeps_the_other() {
        let g = UploadGate::default();
        let k = (0, 0, 16384);
        g.register(k);
        g.register(k);
        g.cancel(k);
        assert!(!g.release(k));
        assert!(g.release(k));
    }

    #[test]
    fn cancel_does_not_touch_other_blocks() {
        let g = UploadGate::default();
        g.register((0, 0, 16384));
        g.register((0, 16384, 16384));
        g.cancel((0, 0, 16384));
        assert!(g.release((0, 16384, 16384)));
        assert!(!g.release((0, 0, 16384)));
    }

    #[test]
    fn counts_stay_in_step_with_cancels_and_releases() {
        let g = UploadGate::default();
        g.register((0, 0, 16384));
        g.register((1, 0, 16384));
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
        let g = UploadGate::default();
        g.register(block_key(&chunk));
        assert!(g.keep(&chunk));
        g.cancel(block_key(&chunk));
        assert!(!g.keep(&chunk));
        assert_eq!(g.queued.load(Ordering::Relaxed), 0);
        // Nothing left for the writer to trip over.
        g.register(block_key(&chunk));
        assert!(g.should_upload(&chunk));
    }
}
