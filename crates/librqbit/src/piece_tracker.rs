//! Coordinates piece download state by wrapping ChunkTracker with inflight piece tracking.
//!
//! This module provides the [`PieceTracker`] type which encapsulates the relationship between
//! queued pieces (in ChunkTracker) and in-flight pieces (being downloaded by a peer).
//!
//! The key invariant maintained is: a piece is in exactly one state at any time:
//! - HAVE (completed)
//! - QUEUED (available to download)
//! - IN_FLIGHT (currently being downloaded)
//! - NOT_NEEDED (not selected for download)

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use buffers::ByteBuf;
use librqbit_core::lengths::ValidPieceIndex;
use peer_binary_protocol::Piece;

use crate::{
    chunk_tracker::{ChunkMarkingResult, ChunkTracker, HaveNeededSelected},
    file_info::FileInfo,
    piece_picker::{PieceAvailability, PiecePicker},
    priority::PieceLevels,
    type_aliases::PeerHandle,
};

/// Tracks a piece currently being downloaded.
#[derive(Debug, Clone)]
pub struct InflightPiece {
    pub peer: PeerHandle,
    pub started: Instant,
    /// A stream asked for this piece. Streams read past priority levels, so when the peer is
    /// slow such a piece is stolen ahead of any level.
    pub streamed: bool,
}

/// An in-flight piece another peer could take over.
struct StealCandidate {
    piece: ValidPieceIndex,
    level: u8,
    streamed: bool,
}

/// Result of attempting to acquire a piece.
#[derive(Debug)]
pub enum AcquireResult {
    /// A new piece was reserved from the queue.
    Reserved(ValidPieceIndex),
    /// A piece was stolen from a slower peer.
    Stolen {
        piece: ValidPieceIndex,
        from_peer: PeerHandle,
    },
    /// No pieces are available for this peer.
    NoneAvailable,
}

/// Parameters for acquiring a piece.
pub struct AcquireRequest<'a, I, P, S>
where
    I: Iterator<Item = ValidPieceIndex>,
    P: Fn(ValidPieceIndex) -> bool,
    S: Fn(ValidPieceIndex) -> bool,
{
    /// The peer requesting a piece.
    pub peer: PeerHandle,
    /// The peer's average piece download time (for steal calculations).
    pub peer_avg_time: Option<Duration>,
    /// Priority pieces to check first (e.g., for streaming).
    pub priority_pieces: I,
    /// Orders the queued pieces (rarest first, sequential, first and last pieces first).
    pub picker: &'a PiecePicker,
    /// How many connected peers have each piece.
    pub availability: &'a PieceAvailability,
    /// Returns true if the peer has the given piece.
    pub peer_has_piece: P,
    /// Returns true if the piece can be stolen (e.g., not locked for writing).
    pub can_steal: S,
}

/// Coordinates piece download state.
///
/// Wraps a [`ChunkTracker`] with tracking of which pieces are currently being downloaded
/// (in-flight) and by which peer. This ensures that:
///
/// - A piece is only assigned to one peer at a time (unless stolen)
/// - Pieces are properly requeued when a peer dies
/// - State transitions maintain invariants
pub struct PieceTracker {
    chunks: ChunkTracker,
    inflight: HashMap<ValidPieceIndex, InflightPiece>,
}

impl PieceTracker {
    // === CONSTRUCTION ===

    /// Create a new PieceTracker wrapping the given ChunkTracker.
    pub fn new(chunks: ChunkTracker) -> Self {
        Self {
            chunks,
            inflight: HashMap::new(),
        }
    }

    /// Read-only access to the underlying ChunkTracker.
    pub fn chunks(&self) -> &ChunkTracker {
        &self.chunks
    }

    /// Consume the PieceTracker, requeuing any in-flight pieces.
    ///
    /// This is used when pausing a torrent - any pieces that were being downloaded
    /// need to be put back in the queue so they can be re-downloaded on resume.
    pub fn into_chunks(mut self) -> ChunkTracker {
        // Requeue all in-flight pieces so they'll be re-downloaded on resume
        for piece in self.inflight.into_keys() {
            self.chunks.mark_piece_broken_if_not_have(piece);
        }
        self.chunks
    }

    /// Attempt to acquire a piece for the requesting peer.
    ///
    /// The acquisition strategy is:
    /// 1. Try to steal a piece from a peer that's 10x slower
    /// 2. Try to reserve a piece from the queue (priority pieces first, then as `picker` orders
    ///    the queue)
    /// 3. Try to steal a piece from a peer that's 3x slower
    ///
    /// A steal takes a piece a stream waits for first, otherwise the highest-priority piece
    /// that qualifies; step 1 gives way when the queue has a piece of a higher priority for
    /// this peer, unless a stream waits for the stolen one.
    ///
    /// If `Stolen` is returned, the caller MUST call `peers.on_steal()` to notify
    /// the old peer and update counters.
    pub fn acquire_piece<I, P, S>(&mut self, mut req: AcquireRequest<I, P, S>) -> AcquireResult
    where
        I: Iterator<Item = ValidPieceIndex>,
        P: Fn(ValidPieceIndex) -> bool,
        S: Fn(ValidPieceIndex) -> bool,
    {
        // The queue pick, once computed. Only needed this early when a steal has to be weighed
        // against it, which never happens while every wanted piece has the same level.
        let mut picked = None;

        // 1. Try steal with 10x threshold (very slow peer)
        if let Some(c) = self.steal_candidate(&req, 10.0) {
            // A stream is waiting for its piece, so a queued one never beats it.
            let beaten = !c.streamed && c.level < self.chunks.queued_top() && {
                let p = self.pick_queued(&req);
                picked = Some(p);
                p.is_some_and(|p| self.chunks.piece_levels().get(p.get_usize()) > c.level)
            };
            if !beaten && let Some(result) = self.steal(c.piece, &req) {
                return result;
            }
        }

        // 2. Try reserve from priority_pieces then queued pieces
        // First check priority pieces that aren't already downloaded or in-flight
        for piece in &mut req.priority_pieces {
            if self.chunks.is_piece_have(piece) {
                continue;
            }
            if let Some(info) = self.inflight.get_mut(&piece) {
                // Reserved for the queue before the stream reached it: the stream waits for it
                // all the same.
                info.streamed = true;
                continue;
            }
            if (req.peer_has_piece)(piece) {
                return self.reserve_piece(piece, req.peer, true);
            }
        }

        // Then the queue (it holds no in-flight pieces), in the picker's order.
        if let Some(piece) = picked.unwrap_or_else(|| self.pick_queued(&req)) {
            return self.reserve_piece(piece, req.peer, false);
        }

        // 3. Try steal with 3x threshold (moderately slow peer)
        if let Some(c) = self.steal_candidate(&req, 3.0)
            && let Some(result) = self.steal(c.piece, &req)
        {
            return result;
        }

        AcquireResult::NoneAvailable
    }

    /// The queued piece the picker chooses for this peer.
    fn pick_queued<I, P, S>(&self, req: &AcquireRequest<I, P, S>) -> Option<ValidPieceIndex>
    where
        I: Iterator<Item = ValidPieceIndex>,
        P: Fn(ValidPieceIndex) -> bool,
        S: Fn(ValidPieceIndex) -> bool,
    {
        let lengths = *self.chunks.get_lengths();
        req.picker
            .pick(
                self.chunks.queued_pieces(),
                self.chunks.piece_levels(),
                self.chunks.queued_top(),
                self.chunks.get_hns().have_bytes,
                req.availability,
                |p| {
                    u32::try_from(p)
                        .ok()
                        .and_then(|p| lengths.validate_piece_index(p))
                        .is_some_and(|p| (req.peer_has_piece)(p))
                },
            )
            .and_then(|p| u32::try_from(p).ok())
            .and_then(|p| lengths.validate_piece_index(p))
    }

    /// Reserve a piece: remove from queue, add to inflight.
    fn reserve_piece(
        &mut self,
        piece: ValidPieceIndex,
        peer: PeerHandle,
        streamed: bool,
    ) -> AcquireResult {
        self.chunks.reserve_needed_piece(piece);
        self.inflight.insert(
            piece,
            InflightPiece {
                peer,
                started: Instant::now(),
                streamed,
            },
        );
        AcquireResult::Reserved(piece)
    }

    /// The piece to steal from a slower peer: of the pieces another peer has been downloading
    /// for `threshold` times this peer's average and that this peer has, a streamed one first,
    /// then the highest level, and inside it the one in flight the longest.
    fn steal_candidate<I, P, S>(
        &self,
        req: &AcquireRequest<I, P, S>,
        threshold: f64,
    ) -> Option<StealCandidate>
    where
        I: Iterator<Item = ValidPieceIndex>,
        P: Fn(ValidPieceIndex) -> bool,
        S: Fn(ValidPieceIndex) -> bool,
    {
        let my_avg = req.peer_avg_time?;
        let min_elapsed = Duration::from_secs_f64(my_avg.as_secs_f64() * threshold);
        let levels = self.chunks.piece_levels();

        // Find the slowest piece from another peer that exceeds threshold
        // and that the stealing peer actually has (can download)
        self.inflight
            .iter()
            .filter(|(_, info)| info.peer != req.peer)
            .filter(|(p, _)| (req.peer_has_piece)(**p))
            .map(|(p, info)| {
                let candidate = StealCandidate {
                    piece: *p,
                    level: levels.get(p.get_usize()),
                    streamed: info.streamed,
                };
                (candidate, info.started.elapsed())
            })
            .filter(|(_, elapsed)| *elapsed >= min_elapsed)
            .max_by_key(|(c, elapsed)| (c.streamed, c.level, *elapsed))
            .map(|(c, _)| c)
    }

    /// Hands an in-flight piece to the requesting peer, unless `can_steal` refuses it.
    fn steal<I, P, S>(
        &mut self,
        piece: ValidPieceIndex,
        req: &AcquireRequest<I, P, S>,
    ) -> Option<AcquireResult>
    where
        I: Iterator<Item = ValidPieceIndex>,
        P: Fn(ValidPieceIndex) -> bool,
        S: Fn(ValidPieceIndex) -> bool,
    {
        // Check can_steal (e.g., per_piece_lock)
        if !(req.can_steal)(piece) {
            return None;
        }

        // Update ownership (piece stays in inflight, just changes owner)
        let info = self.inflight.get_mut(&piece)?;
        let old_peer = info.peer;
        info.peer = req.peer;
        info.started = Instant::now();

        Some(AcquireResult::Stolen {
            piece,
            from_peer: old_peer,
        })
    }

    // === PIECE COMPLETION ===

    /// Remove piece from inflight tracking (e.g., after all chunks received).
    ///
    /// Returns download duration if piece was in-flight.
    /// Note: Does NOT mark the piece as downloaded - caller should do hash check
    /// and then call `mark_piece_hash_ok` or `mark_piece_hash_failed`.
    pub fn take_inflight(&mut self, piece: ValidPieceIndex) -> Option<Duration> {
        let inflight = self.inflight.remove(&piece)?;
        Some(inflight.started.elapsed())
    }

    /// Mark piece as downloaded after successful hash verification.
    pub fn mark_piece_hash_ok(&mut self, piece: ValidPieceIndex) {
        self.chunks.mark_piece_downloaded(piece);
    }

    /// Mark piece as failed after hash verification failure - requeues the piece.
    pub fn mark_piece_hash_failed(&mut self, piece: ValidPieceIndex) {
        self.chunks.mark_piece_broken_if_not_have(piece);
    }

    /// Release all pieces owned by a peer (on peer death).
    ///
    /// Moves all pieces owned by the peer from IN_FLIGHT back to QUEUED.
    /// Returns the number of pieces released.
    pub fn release_pieces_owned_by(&mut self, peer: PeerHandle) -> usize {
        // Collect pieces to release (can't modify while iterating)
        let pieces_to_release: Vec<_> = self
            .inflight
            .iter()
            .filter(|(_, info)| info.peer == peer)
            .map(|(p, _)| *p)
            .collect();

        let count = pieces_to_release.len();
        for piece in pieces_to_release {
            self.inflight.remove(&piece);
            self.chunks.mark_piece_broken_if_not_have(piece);
        }
        count
    }

    // === QUERIES ===

    /// Get the inflight info for a piece, if it's currently being downloaded.
    pub fn get_inflight(&self, piece: ValidPieceIndex) -> Option<&InflightPiece> {
        self.inflight.get(&piece)
    }

    /// Check if a piece is currently in-flight.
    #[allow(dead_code)]
    pub fn is_inflight(&self, piece: ValidPieceIndex) -> bool {
        self.inflight.contains_key(&piece)
    }

    /// Get the number of pieces currently in-flight.
    #[allow(dead_code)]
    pub fn inflight_count(&self) -> usize {
        self.inflight.len()
    }

    // === PASS-THROUGH METHODS ===

    /// Mark a chunk as downloaded. Returns the result indicating if the piece is complete.
    pub fn mark_chunk_downloaded(
        &mut self,
        piece: &Piece<ByteBuf<'_>>,
    ) -> Option<ChunkMarkingResult> {
        self.chunks.mark_chunk_downloaded(piece)
    }

    /// Changes the effective priority of the pieces.
    ///
    /// Pieces that become wanted are queued; pieces whose level drops to 0 leave the queue, and
    /// the ones of them in flight are released: they leave the in-flight map and their partly
    /// written chunks are forgotten. Returns the new byte accounting and the released pieces
    /// with the peers that were downloading them, whose requests the caller must cancel.
    /// Pieces a stream asked for stay in flight whatever their level: the stream reads them.
    pub fn set_piece_levels(
        &mut self,
        levels: PieceLevels,
    ) -> anyhow::Result<(HaveNeededSelected, Vec<(ValidPieceIndex, PeerHandle)>)> {
        let released: Vec<(ValidPieceIndex, PeerHandle)> = self
            .inflight
            .iter()
            .filter(|(p, info)| !info.streamed && levels.get(p.get_usize()) == 0)
            .map(|(p, info)| (*p, info.peer))
            .collect();
        let inflight = &self.inflight;
        let hns = self
            .chunks
            .set_piece_levels(levels, |p| inflight.contains_key(&p))?;
        for (piece, _) in &released {
            self.inflight.remove(piece);
            // Not selected any more, so this forgets its chunks without queueing it.
            self.chunks.mark_piece_broken_if_not_have(*piece);
        }
        Ok((hns, released))
    }

    /// Update per-file have bytes when a piece completes. Returns remaining bytes for the file.
    pub fn update_file_have_on_piece_completed(
        &mut self,
        piece_id: ValidPieceIndex,
        file_id: usize,
        file_info: &FileInfo,
    ) -> u64 {
        self.chunks
            .update_file_have_on_piece_completed(piece_id, file_id, file_info)
    }

    /// Flush the have pieces bitfield to disk.
    pub fn flush_have_pieces(&mut self, flush_async: bool) -> anyhow::Result<()> {
        self.chunks.get_have_pieces_mut().flush(flush_async)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bitv::BitV as BitVTrait,
        type_aliases::{BF, FileInfos},
    };
    use librqbit_core::lengths::Lengths;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn peer(id: u8) -> PeerHandle {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, id)), 6881)
    }

    /// Create a simple ChunkTracker for testing.
    /// Creates a torrent with the specified number of pieces, all selected.
    fn make_test_chunk_tracker(num_pieces: u32) -> ChunkTracker {
        // Create a simple single-file torrent
        let piece_length = 16384u32; // 16KB pieces
        let total_length = piece_length as u64 * num_pieces as u64;

        let lengths = Lengths::new(total_length, piece_length).unwrap();

        let bf_len = lengths.piece_bitfield_bytes();

        // No pieces downloaded yet (empty have)
        let have = BF::from_boxed_slice(vec![0u8; bf_len].into_boxed_slice());

        // All pieces selected
        let levels = PieceLevels::new(vec![1u8; num_pieces as usize].into_boxed_slice());

        // Single file spanning all pieces
        let file_infos = vec![crate::file_info::FileInfo {
            relative_filename: "test.dat".into(),
            offset_in_torrent: 0,
            len: total_length,
            piece_range: 0..num_pieces,
            attrs: Default::default(),
        }];

        ChunkTracker::new(have.into_dyn(), levels, lengths, &file_infos).unwrap()
    }

    fn make_test_file_infos(num_pieces: u32) -> FileInfos {
        let piece_length = 16384u64;
        vec![crate::file_info::FileInfo {
            relative_filename: "test.dat".into(),
            offset_in_torrent: 0,
            len: piece_length * num_pieces as u64,
            piece_range: 0..num_pieces,
            attrs: Default::default(),
        }]
    }

    /// A picker that hands out pieces in index order, so tests can predict them.
    fn make_sequential_picker(file_infos: &FileInfos) -> (PiecePicker, PieceAvailability) {
        let total: u32 = file_infos
            .iter()
            .map(|f| f.piece_range.end)
            .max()
            .unwrap_or(0);
        let lengths = Lengths::new(16384u64 * u64::from(total), 16384).unwrap();
        let picking = crate::PiecePicking {
            sequential: true,
            ..Default::default()
        };
        (
            PiecePicker::new(picking, lengths, file_infos, &vec![1; file_infos.len()]),
            PieceAvailability::new(total),
        )
    }

    #[test]
    fn test_new_piece_tracker() {
        let chunks = make_test_chunk_tracker(10);
        let tracker = PieceTracker::new(chunks);
        assert_eq!(tracker.inflight_count(), 0);
    }

    #[test]
    fn test_reserve_piece_from_queue() {
        let chunks = make_test_chunk_tracker(5);
        let mut tracker = PieceTracker::new(chunks);

        let file_infos = make_test_file_infos(5);
        let (picker, availability) = make_sequential_picker(&file_infos);

        let result = tracker.acquire_piece(AcquireRequest {
            peer: peer(1),
            peer_avg_time: None,
            priority_pieces: std::iter::empty(),
            picker: &picker,
            availability: &availability,
            peer_has_piece: |_| true, // Peer has all pieces
            can_steal: |_| true,
        });

        // Should reserve piece 0 (first in queue)
        match result {
            AcquireResult::Reserved(piece) => {
                assert_eq!(piece.get(), 0);
                assert!(tracker.is_inflight(piece));
                assert_eq!(tracker.inflight_count(), 1);
            }
            _ => panic!("Expected Reserved, got {:?}", result),
        }
    }

    #[test]
    fn test_reserve_filters_by_peer_has_piece() {
        let chunks = make_test_chunk_tracker(5);
        let mut tracker = PieceTracker::new(chunks);

        let file_infos = make_test_file_infos(5);
        let (picker, availability) = make_sequential_picker(&file_infos);

        // Peer only has piece 2 and later
        // Note: the sequential picker hands out pieces in index order,
        // so with filter >= 2 we get 2
        let result = tracker.acquire_piece(AcquireRequest {
            peer: peer(1),
            peer_avg_time: None,
            priority_pieces: std::iter::empty(),
            picker: &picker,
            availability: &availability,
            peer_has_piece: |p| p.get() >= 2,
            can_steal: |_| true,
        });

        match result {
            AcquireResult::Reserved(piece) => {
                // Got piece 2 (first one peer has in index order)
                assert!(piece.get() >= 2, "Should have gotten a piece >= 2");
            }
            _ => panic!("Expected Reserved, got {:?}", result),
        }
    }

    #[test]
    fn test_complete_piece() {
        let chunks = make_test_chunk_tracker(5);
        let mut tracker = PieceTracker::new(chunks);

        let file_infos = make_test_file_infos(5);
        let (picker, availability) = make_sequential_picker(&file_infos);

        // Reserve a piece first
        let result = tracker.acquire_piece(AcquireRequest {
            peer: peer(1),
            peer_avg_time: None,
            priority_pieces: std::iter::empty(),
            picker: &picker,
            availability: &availability,
            peer_has_piece: |_| true,
            can_steal: |_| true,
        });

        let piece = match result {
            AcquireResult::Reserved(p) => p,
            _ => panic!("Expected Reserved"),
        };

        // Complete the piece (take_inflight + hash check + mark_piece_hash_ok)
        let duration = tracker.take_inflight(piece);
        assert!(duration.is_some());
        assert!(!tracker.is_inflight(piece));
        // Simulate successful hash check
        tracker.mark_piece_hash_ok(piece);
        assert!(tracker.chunks().is_piece_have(piece));
    }

    #[test]
    fn test_fail_piece_requeues() {
        let chunks = make_test_chunk_tracker(5);
        let mut tracker = PieceTracker::new(chunks);

        let file_infos = make_test_file_infos(5);
        let (picker, availability) = make_sequential_picker(&file_infos);

        // Reserve piece 0
        let result = tracker.acquire_piece(AcquireRequest {
            peer: peer(1),
            peer_avg_time: None,
            priority_pieces: std::iter::empty(),
            picker: &picker,
            availability: &availability,
            peer_has_piece: |_| true,
            can_steal: |_| true,
        });

        let piece = match result {
            AcquireResult::Reserved(p) => p,
            _ => panic!("Expected Reserved"),
        };

        assert!(tracker.is_inflight(piece));

        // Fail the piece (take_inflight + hash check fails + mark_piece_hash_failed)
        let duration = tracker.take_inflight(piece);
        assert!(duration.is_some());
        // Simulate failed hash check
        tracker.mark_piece_hash_failed(piece);

        // Should no longer be in-flight
        assert!(!tracker.is_inflight(piece));
        // Should not be in have
        assert!(!tracker.chunks().is_piece_have(piece));
        // Should be back in queue - verify by trying to reserve it again
        let result2 = tracker.acquire_piece(AcquireRequest {
            peer: peer(2),
            peer_avg_time: None,
            priority_pieces: std::iter::empty(),
            picker: &picker,
            availability: &availability,
            peer_has_piece: |p| p == piece, // Only has the failed piece
            can_steal: |_| true,
        });

        match result2 {
            AcquireResult::Reserved(p) => assert_eq!(p, piece),
            _ => panic!("Expected piece to be re-reservable after fail"),
        }
    }

    #[test]
    fn test_release_pieces_owned_by_peer() {
        let chunks = make_test_chunk_tracker(5);
        let mut tracker = PieceTracker::new(chunks);

        let file_infos = make_test_file_infos(5);
        let (picker, availability) = make_sequential_picker(&file_infos);

        let peer_a = peer(1);
        let peer_b = peer(2);

        // Peer A reserves first two pieces (index order with the sequential picker)
        // So peer A gets pieces 0 and 1
        let piece_a1 = match tracker.acquire_piece(AcquireRequest {
            peer: peer_a,
            peer_avg_time: None,
            priority_pieces: std::iter::empty(),
            picker: &picker,
            availability: &availability,
            peer_has_piece: |_| true,
            can_steal: |_| true,
        }) {
            AcquireResult::Reserved(p) => p,
            _ => panic!("Expected Reserved"),
        };
        let piece_a2 = match tracker.acquire_piece(AcquireRequest {
            peer: peer_a,
            peer_avg_time: None,
            priority_pieces: std::iter::empty(),
            picker: &picker,
            availability: &availability,
            peer_has_piece: |_| true,
            can_steal: |_| true,
        }) {
            AcquireResult::Reserved(p) => p,
            _ => panic!("Expected Reserved"),
        };

        // Peer B reserves next piece
        let piece_b = match tracker.acquire_piece(AcquireRequest {
            peer: peer_b,
            peer_avg_time: None,
            priority_pieces: std::iter::empty(),
            picker: &picker,
            availability: &availability,
            peer_has_piece: |_| true,
            can_steal: |_| true,
        }) {
            AcquireResult::Reserved(p) => p,
            _ => panic!("Expected Reserved"),
        };

        assert_eq!(tracker.inflight_count(), 3);
        assert!(tracker.is_inflight(piece_a1));
        assert!(tracker.is_inflight(piece_a2));
        assert!(tracker.is_inflight(piece_b));

        // Peer A dies
        let released = tracker.release_pieces_owned_by(peer_a);
        assert_eq!(released, 2);
        assert_eq!(tracker.inflight_count(), 1); // Only peer B's piece remains

        // Verify peer B's piece is still in-flight
        assert!(tracker.is_inflight(piece_b));
        // Verify peer A's pieces are no longer in-flight
        assert!(!tracker.is_inflight(piece_a1));
        assert!(!tracker.is_inflight(piece_a2));
    }

    #[test]
    fn test_into_chunks_requeues_inflight() {
        let chunks = make_test_chunk_tracker(5);
        let mut tracker = PieceTracker::new(chunks);

        let file_infos = make_test_file_infos(5);
        let (picker, availability) = make_sequential_picker(&file_infos);

        // Reserve pieces 0 and 1
        tracker.acquire_piece(AcquireRequest {
            peer: peer(1),
            peer_avg_time: None,
            priority_pieces: std::iter::empty(),
            picker: &picker,
            availability: &availability,
            peer_has_piece: |_| true,
            can_steal: |_| true,
        });
        tracker.acquire_piece(AcquireRequest {
            peer: peer(1),
            peer_avg_time: None,
            priority_pieces: std::iter::empty(),
            picker: &picker,
            availability: &availability,
            peer_has_piece: |_| true,
            can_steal: |_| true,
        });

        assert_eq!(tracker.inflight_count(), 2);

        // Convert back to chunks (simulates pause)
        let chunks = tracker.into_chunks();

        // Create a new tracker and verify pieces are back in queue
        let mut new_tracker = PieceTracker::new(chunks);
        let result = new_tracker.acquire_piece(AcquireRequest {
            peer: peer(2),
            peer_avg_time: None,
            priority_pieces: std::iter::empty(),
            picker: &picker,
            availability: &availability,
            peer_has_piece: |_| true,
            can_steal: |_| true,
        });

        // Should get piece 0 again (was requeued)
        match result {
            AcquireResult::Reserved(p) => assert_eq!(p.get(), 0),
            _ => panic!("Expected to reserve piece 0 after into_chunks"),
        }
    }

    #[test]
    fn test_priority_pieces_checked_first() {
        let chunks = make_test_chunk_tracker(5);
        let mut tracker = PieceTracker::new(chunks);

        let file_infos = make_test_file_infos(5);
        let (picker, availability) = make_sequential_picker(&file_infos);

        // Priority pieces: piece 3, then piece 2
        let piece3 = tracker
            .chunks()
            .get_lengths()
            .validate_piece_index(3)
            .unwrap();
        let piece2 = tracker
            .chunks()
            .get_lengths()
            .validate_piece_index(2)
            .unwrap();
        let priority = vec![piece3, piece2];

        let result = tracker.acquire_piece(AcquireRequest {
            peer: peer(1),
            peer_avg_time: None,
            priority_pieces: priority.into_iter(),
            picker: &picker,
            availability: &availability,
            peer_has_piece: |_| true,
            can_steal: |_| true,
        });

        // Should get piece 3 (first priority piece)
        match result {
            AcquireResult::Reserved(p) => assert_eq!(p.get(), 3),
            _ => panic!("Expected Reserved(3), got {:?}", result),
        }
    }

    #[test]
    fn test_none_available_when_no_pieces() {
        let chunks = make_test_chunk_tracker(5);
        let mut tracker = PieceTracker::new(chunks);

        let file_infos = make_test_file_infos(5);
        let (picker, availability) = make_sequential_picker(&file_infos);

        // Peer has no pieces
        let result = tracker.acquire_piece(AcquireRequest {
            peer: peer(1),
            peer_avg_time: None,
            priority_pieces: std::iter::empty(),
            picker: &picker,
            availability: &availability,
            peer_has_piece: |_| false, // Peer has nothing
            can_steal: |_| true,
        });

        match result {
            AcquireResult::NoneAvailable => {}
            _ => panic!("Expected NoneAvailable, got {:?}", result),
        }
    }

    #[test]
    fn test_take_inflight_nonexistent_piece_returns_none() {
        let chunks = make_test_chunk_tracker(5);
        let mut tracker = PieceTracker::new(chunks);
        let piece = tracker
            .chunks()
            .get_lengths()
            .validate_piece_index(0)
            .unwrap();

        // Try to take a piece that's not in-flight
        let result = tracker.take_inflight(piece);
        assert!(result.is_none());
    }

    #[test]
    fn test_steal_only_pieces_peer_has() {
        // This test verifies the fix for a bug where try_steal didn't check
        // if the stealing peer actually has the piece in their bitfield.
        // Without this check, a peer could "steal" a piece they can't download,
        // leaving it stuck in inflight forever.

        let chunks = make_test_chunk_tracker(5);
        let mut tracker = PieceTracker::new(chunks);

        let file_infos = make_test_file_infos(5);
        let (picker, availability) = make_sequential_picker(&file_infos);

        let peer_a = peer(1);
        let peer_b = peer(2);

        // Peer A reserves pieces 0 and 1 (first two in index order)
        let piece_0 = match tracker.acquire_piece(AcquireRequest {
            peer: peer_a,
            peer_avg_time: None,
            priority_pieces: std::iter::empty(),
            picker: &picker,
            availability: &availability,
            peer_has_piece: |_| true,
            can_steal: |_| true,
        }) {
            AcquireResult::Reserved(p) => {
                assert_eq!(p.get(), 0);
                p
            }
            _ => panic!("Expected Reserved"),
        };

        let piece_1 = match tracker.acquire_piece(AcquireRequest {
            peer: peer_a,
            peer_avg_time: None,
            priority_pieces: std::iter::empty(),
            picker: &picker,
            availability: &availability,
            peer_has_piece: |_| true,
            can_steal: |_| true,
        }) {
            AcquireResult::Reserved(p) => {
                assert_eq!(p.get(), 1);
                p
            }
            _ => panic!("Expected Reserved"),
        };

        // Sleep briefly so pieces become stealable
        std::thread::sleep(Duration::from_millis(5));

        // Peer B tries to acquire with:
        // - Very short avg_time (1ms) so 3x threshold = 3ms < 5ms elapsed
        // - peer_has_piece returns true ONLY for piece 1, NOT piece 0
        let result = tracker.acquire_piece(AcquireRequest {
            peer: peer_b,
            peer_avg_time: Some(Duration::from_millis(1)),
            priority_pieces: std::iter::empty(),
            picker: &picker,
            availability: &availability,
            peer_has_piece: |p| p.get() == 1, // Peer B only has piece 1
            can_steal: |_| true,
        });

        // Should steal piece 1 (which peer B has), NOT piece 0 (which peer B doesn't have)
        match result {
            AcquireResult::Stolen { piece, from_peer } => {
                assert_eq!(piece, piece_1, "Should steal piece 1 (the one peer B has)");
                assert_eq!(from_peer, peer_a);
                // Verify piece 0 is still owned by peer A (wasn't stolen)
                assert_eq!(tracker.get_inflight(piece_0).unwrap().peer, peer_a);
            }
            _ => panic!("Expected Stolen, got {:?}", result),
        }
    }

    /// A single-file torrent with one piece per entry of `levels`, at those levels.
    fn make_leveled_chunk_tracker(levels: &[u8]) -> ChunkTracker {
        let num_pieces = u32::try_from(levels.len()).unwrap();
        let lengths = Lengths::new(16384 * u64::from(num_pieces), 16384).unwrap();
        let have =
            BF::from_boxed_slice(vec![0u8; lengths.piece_bitfield_bytes()].into_boxed_slice());
        ChunkTracker::new(
            have.into_dyn(),
            PieceLevels::new(levels.into()),
            lengths,
            &make_test_file_infos(num_pieces),
        )
        .unwrap()
    }

    /// Peer A downloads piece 0 (handed to it directly, not through a stream) and then the
    /// piece the picker gives it.
    fn reserve_0_then_pick(
        tracker: &mut PieceTracker,
        picker: &PiecePicker,
        availability: &PieceAvailability,
    ) -> ValidPieceIndex {
        let piece_0 = tracker
            .chunks
            .get_lengths()
            .validate_piece_index(0)
            .unwrap();
        let first = tracker.reserve_piece(piece_0, peer(1), false);
        assert!(matches!(first, AcquireResult::Reserved(p) if p == piece_0));
        match tracker.acquire_piece(AcquireRequest {
            peer: peer(1),
            peer_avg_time: None,
            priority_pieces: std::iter::empty(),
            picker,
            availability,
            peer_has_piece: |_| true,
            can_steal: |_| true,
        }) {
            AcquireResult::Reserved(p) => p,
            other => panic!("expected Reserved, got {other:?}"),
        }
    }

    #[test]
    fn test_steal_takes_the_highest_level_piece() {
        let mut tracker = PieceTracker::new(make_leveled_chunk_tracker(&[1, 7, 1]));
        let file_infos = make_test_file_infos(3);
        let (picker, availability) = make_sequential_picker(&file_infos);
        let high = reserve_0_then_pick(&mut tracker, &picker, &availability);
        assert_eq!(high.get(), 1);

        std::thread::sleep(Duration::from_millis(5));

        // Piece 0 has been in flight longer, but piece 1 is the more important one. Piece 2 is
        // queued, but at a lower level than the steal.
        let result = tracker.acquire_piece(AcquireRequest {
            peer: peer(2),
            peer_avg_time: Some(Duration::from_micros(100)),
            priority_pieces: std::iter::empty(),
            picker: &picker,
            availability: &availability,
            peer_has_piece: |_| true,
            can_steal: |_| true,
        });
        match result {
            AcquireResult::Stolen { piece, from_peer } => {
                assert_eq!(piece, high);
                assert_eq!(from_peer, peer(1));
            }
            other => panic!("expected Stolen, got {other:?}"),
        }
    }

    #[test]
    fn test_steal_gives_way_to_a_higher_queued_piece() {
        let mut tracker = PieceTracker::new(make_leveled_chunk_tracker(&[1, 1, 7]));
        let file_infos = make_test_file_infos(3);
        let (picker, availability) = make_sequential_picker(&file_infos);
        // Peer A holds pieces 0 and 2; then piece 2 is given back, as when its peer dies.
        reserve_0_then_pick(&mut tracker, &picker, &availability);
        assert_eq!(tracker.release_pieces_owned_by(peer(1)), 2);
        let piece_0 = tracker
            .chunks
            .get_lengths()
            .validate_piece_index(0)
            .unwrap();
        assert!(matches!(
            tracker.reserve_piece(piece_0, peer(1), false),
            AcquireResult::Reserved(p) if p == piece_0
        ));

        std::thread::sleep(Duration::from_millis(5));

        // Piece 0 is slow enough to steal, but queued piece 2 is worth more.
        let result = tracker.acquire_piece(AcquireRequest {
            peer: peer(2),
            peer_avg_time: Some(Duration::from_micros(100)),
            priority_pieces: std::iter::empty(),
            picker: &picker,
            availability: &availability,
            peer_has_piece: |_| true,
            can_steal: |_| true,
        });
        assert!(
            matches!(result, AcquireResult::Reserved(p) if p.get() == 2),
            "expected piece 2 from the queue, got {result:?}"
        );
        assert_eq!(tracker.get_inflight(piece_0).unwrap().peer, peer(1));
    }

    #[test]
    fn test_level_zero_pieces_are_never_reserved() {
        let mut tracker = PieceTracker::new(make_leveled_chunk_tracker(&[0, 1, 0]));
        let file_infos = make_test_file_infos(3);
        let (picker, availability) = make_sequential_picker(&file_infos);
        let mut got = Vec::new();
        loop {
            match tracker.acquire_piece(AcquireRequest {
                peer: peer(1),
                peer_avg_time: None,
                priority_pieces: std::iter::empty(),
                picker: &picker,
                availability: &availability,
                peer_has_piece: |_| true,
                can_steal: |_| true,
            }) {
                AcquireResult::Reserved(p) => got.push(p.get()),
                AcquireResult::NoneAvailable => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(got, vec![1]);
    }

    #[test]
    fn test_lowering_to_zero_releases_only_the_unwanted_in_flight_pieces() {
        let mut tracker = PieceTracker::new(make_leveled_chunk_tracker(&[1, 1, 1, 0]));
        let file_infos = make_test_file_infos(4);
        let (picker, availability) = make_sequential_picker(&file_infos);
        let reserve = |tracker: &mut PieceTracker, who: u8, piece: u32, streamed: bool| {
            let piece = tracker
                .chunks
                .get_lengths()
                .validate_piece_index(piece)
                .unwrap();
            let got = if streamed {
                tracker.acquire_piece(AcquireRequest {
                    peer: peer(who),
                    peer_avg_time: None,
                    priority_pieces: std::iter::once(piece),
                    picker: &picker,
                    availability: &availability,
                    peer_has_piece: |_| true,
                    can_steal: |_| false,
                })
            } else {
                tracker.reserve_piece(piece, peer(who), false)
            };
            assert!(matches!(got, AcquireResult::Reserved(p) if p == piece));
        };
        reserve(&mut tracker, 1, 0, false);
        reserve(&mut tracker, 2, 1, false);
        // Piece 3 is at 0 and in flight: a stream asked for it.
        reserve(&mut tracker, 3, 3, true);

        let (hns, released) = tracker
            .set_piece_levels(PieceLevels::new(vec![1u8, 0, 0, 0].into_boxed_slice()))
            .unwrap();
        assert_eq!(
            released
                .iter()
                .map(|(p, who)| (p.get(), *who))
                .collect::<Vec<_>>(),
            vec![(1, peer(2))]
        );
        assert_eq!(hns.selected_bytes, 16384);
        assert_eq!(hns.needed_bytes, 16384);
        assert!(tracker.get_inflight(released[0].0).is_none());
        // The wanted piece and the stream's piece stay with their peers.
        let valid = |i| {
            tracker
                .chunks
                .get_lengths()
                .validate_piece_index(i)
                .unwrap()
        };
        assert_eq!(tracker.get_inflight(valid(0)).unwrap().peer, peer(1));
        assert_eq!(tracker.get_inflight(valid(3)).unwrap().peer, peer(3));
        // Nothing at 0 is queued.
        assert_eq!(tracker.chunks.queued_pieces().count_ones(), 0);
    }

    #[test]
    fn test_a_stalled_stream_piece_is_stolen_ahead_of_any_level() {
        // Piece 0 belongs to a skipped file a stream reads; pieces 1 and 2 are queued at 7.
        let mut tracker = PieceTracker::new(make_leveled_chunk_tracker(&[0, 7, 7]));
        let file_infos = make_test_file_infos(3);
        let (picker, availability) = make_sequential_picker(&file_infos);
        let piece_0 = tracker
            .chunks
            .get_lengths()
            .validate_piece_index(0)
            .unwrap();
        let first = tracker.acquire_piece(AcquireRequest {
            peer: peer(1),
            peer_avg_time: None,
            priority_pieces: std::iter::once(piece_0),
            picker: &picker,
            availability: &availability,
            peer_has_piece: |_| true,
            can_steal: |_| true,
        });
        assert!(matches!(first, AcquireResult::Reserved(p) if p == piece_0));

        std::thread::sleep(Duration::from_millis(5));

        // A fast peer takes the stream's piece over, although the queue has higher levels.
        let result = tracker.acquire_piece(AcquireRequest {
            peer: peer(2),
            peer_avg_time: Some(Duration::from_micros(100)),
            priority_pieces: std::iter::empty(),
            picker: &picker,
            availability: &availability,
            peer_has_piece: |_| true,
            can_steal: |_| true,
        });
        assert!(
            matches!(result, AcquireResult::Stolen { piece, from_peer } if piece == piece_0 && from_peer == peer(1)),
            "expected the stream's piece to be stolen, got {result:?}"
        );
    }

    #[test]
    fn test_a_queue_piece_a_stream_reaches_survives_lowering_to_zero() {
        let mut tracker = PieceTracker::new(make_leveled_chunk_tracker(&[1, 1]));
        let file_infos = make_test_file_infos(2);
        let (picker, availability) = make_sequential_picker(&file_infos);
        let piece_0 = tracker
            .chunks
            .get_lengths()
            .validate_piece_index(0)
            .unwrap();
        tracker.reserve_piece(piece_0, peer(1), false);
        // A stream starts reading piece 0 while it is in flight; peer 2 gets piece 1 instead.
        let next = tracker.acquire_piece(AcquireRequest {
            peer: peer(2),
            peer_avg_time: None,
            priority_pieces: std::iter::once(piece_0),
            picker: &picker,
            availability: &availability,
            peer_has_piece: |_| true,
            can_steal: |_| true,
        });
        assert!(matches!(next, AcquireResult::Reserved(p) if p.get() == 1));
        assert!(tracker.get_inflight(piece_0).unwrap().streamed);

        let (_, released) = tracker
            .set_piece_levels(PieceLevels::new(vec![0u8, 0].into_boxed_slice()))
            .unwrap();
        assert_eq!(
            released
                .iter()
                .map(|(p, who)| (p.get(), *who))
                .collect::<Vec<_>>(),
            vec![(1, peer(2))]
        );
        assert_eq!(tracker.get_inflight(piece_0).unwrap().peer, peer(1));
    }
}
