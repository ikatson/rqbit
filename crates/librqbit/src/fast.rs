//! BEP 6 (Fast Extension) state and helpers that do not need the rest of the torrent.
//!
//! The wire messages live in `peer_binary_protocol`. What is here is the canonical allowed-fast
//! set, the limits shared by the upload and download sides, and the per-connection bookkeeping
//! of both sides ([`FastDownload`], [`FastUpload`]), kept free of locks and I/O so the peer
//! handler stays a thin caller.

use std::{
    collections::{HashSet, VecDeque},
    net::Ipv4Addr,
    time::{Duration, Instant},
};

use librqbit_core::{Id20, lengths::ValidPieceIndex};
use sha1w::{ISha1, Sha1};

use crate::upload_gate::BlockKey;

/// Pieces in the allowed-fast set we offer a peer. The BEP currently sets this to 10.
pub(crate) const ALLOWED_FAST_K: u32 = 10;

/// A peer with at least this many pieces is no longer a newcomer: we do not offer it an
/// allowed-fast set (the BEP's purpose is to let a newcomer bootstrap; its own implementation
/// stops serving the set once the peer has more than *k* pieces). A set already offered stays
/// valid, each of its blocks is served once while the peer is choked.
pub(crate) const ALLOWED_FAST_MAX_PEER_PIECES: u32 = ALLOWED_FAST_K;

/// Rejects sent per choke period (the budget is renewed whenever we choke or unchoke the peer)
/// for requests we turn down on arrival: the peer is choked, its queue is full, or we do not
/// have the piece. Past it such requests are dropped silently: a peer could otherwise keep the
/// connection busy by asking for rejects forever. libtorrent caps the same flood. Rejects for
/// requests we had accepted (dropped by a choke, cancelled) are not counted: those are bounded
/// by the upload queue.
pub(crate) const MAX_REJECTS_PER_CHOKE: u32 = 100;

/// Allowed-fast indexes remembered from one peer. *k* is 10; the cap only bounds a peer that
/// sends the message without end.
pub(crate) const MAX_ALLOWED_FAST_REMEMBERED: usize = 32;

/// Suggest-piece indexes remembered from one peer, most recent first.
pub(crate) const MAX_SUGGESTIONS: usize = 16;

/// A piece a peer rejected is not asked of that peer again for this long, unless the peer
/// unchokes us in between (a choke explains the rejects). Without it a peer that rejects while
/// unchoked would get the same request again right away, in a tight loop.
pub(crate) const REJECTED_PIECE_BACKOFF: Duration = Duration::from_secs(30);

/// Rejected pieces remembered per peer for [`REJECTED_PIECE_BACKOFF`]; the oldest is forgotten
/// first.
pub(crate) const MAX_REJECTED_REMEMBERED: usize = 64;

/// How long a peer that choked us has to answer what we asked it for with `Piece` or
/// `RejectRequest` before the requests are given back to the queue, as they are right away
/// without the extension.
///
/// Half the read timeout: the reader asks [`may_be_silent`] after every half read timeout of
/// silence, and a silent peer is given up on after a full one. So by the first question after a
/// choke followed by silence the requests are overdue: they are given back, and the silence is
/// excused, before the reader would give up on the peer. A peer that keeps talking without
/// answering is caught when its next message arrives.
///
/// [`may_be_silent`]: crate::peer_connection::PeerConnectionHandler::may_be_silent
pub(crate) fn choke_answer_timeout(read_write_timeout: Duration) -> Duration {
    read_write_timeout / 2
}

/// The canonical allowed-fast set (BEP 6) for an IPv4 peer.
///
/// The BEP defines the algorithm for IPv4 only. Returns the indexes in generation order, so a
/// smaller *k* is a prefix of a larger one. An empty torrent yields an empty set.
pub(crate) fn allowed_fast_set(
    ip: Ipv4Addr,
    info_hash: &Id20,
    num_pieces: u32,
    k: u32,
) -> Vec<u32> {
    if num_pieces == 0 || k == 0 {
        return Vec::new();
    }
    // The most significant three octets, so two addresses on the same /24 share a set.
    let masked = u32::from(ip) & 0xffff_ff00;
    let mut x = Vec::with_capacity(24);
    x.extend_from_slice(&masked.to_be_bytes());
    x.extend_from_slice(&info_hash.0);

    // A torrent with fewer pieces than k has all of them in the set; without the cap the loop
    // below would never find k distinct indexes.
    let k = k.min(num_pieces) as usize;
    let mut set = Vec::with_capacity(k);
    while set.len() < k {
        let mut hash = Sha1::new();
        hash.update(&x);
        x = hash.finish().to_vec();
        for word in x.as_chunks::<4>().0 {
            if set.len() >= k {
                break;
            }
            let y = u32::from_be_bytes(*word);
            let index = y % num_pieces;
            if !set.contains(&index) {
                set.push(index);
            }
        }
    }
    set
}

/// The downloading side of one connection with the Fast Extension: what the peer told us
/// (allowed-fast pieces, suggestions, rejects) and what it still owes us after a choke.
///
/// Every list is bounded: by a constant, or (`give_back`) by the pieces we asked the peer for.
#[derive(Debug, Default)]
pub(crate) struct FastDownload {
    // Pieces the peer lets us download while it chokes us, in arrival order.
    allowed_fast: Vec<ValidPieceIndex>,
    // Pieces the peer suggested, most recent first.
    suggested: VecDeque<ValidPieceIndex>,
    // Pieces the peer rejected a block of, and when; oldest first.
    rejected: VecDeque<(ValidPieceIndex, Instant)>,
    // Pieces we own that go back to the queue once the last block of them in flight to the
    // peer is answered.
    give_back: Vec<ValidPieceIndex>,
    // Since when the peer, which chokes us, owes us answers for the blocks in flight.
    choked_at: Option<Instant>,
}

impl FastDownload {
    /// Remembers a piece of an `AllowedFast` message. Returns whether it was new; a piece past
    /// [`MAX_ALLOWED_FAST_REMEMBERED`] is not remembered.
    pub fn add_allowed_fast(&mut self, piece: ValidPieceIndex) -> bool {
        if self.allowed_fast.len() >= MAX_ALLOWED_FAST_REMEMBERED
            || self.allowed_fast.contains(&piece)
        {
            return false;
        }
        self.allowed_fast.push(piece);
        true
    }

    /// The pieces the peer allows us to download while it chokes us.
    pub fn allowed_fast(&self) -> &[ValidPieceIndex] {
        &self.allowed_fast
    }

    pub fn is_allowed_fast(&self, piece: ValidPieceIndex) -> bool {
        self.allowed_fast.contains(&piece)
    }

    /// Forgets the allowed-fast set: the peer did not serve it in time, so it is not asked for
    /// it again until it sends a new one.
    pub fn forget_allowed_fast(&mut self) {
        self.allowed_fast.clear();
    }

    /// Remembers a `SuggestPiece`: it goes first, ahead of older suggestions.
    pub fn add_suggestion(&mut self, piece: ValidPieceIndex) {
        self.suggested.retain(|p| *p != piece);
        self.suggested.push_front(piece);
        self.suggested.truncate(MAX_SUGGESTIONS);
    }

    /// The suggested pieces, most recent first.
    pub fn suggestions(&self) -> impl Iterator<Item = ValidPieceIndex> + '_ {
        self.suggested.iter().copied()
    }

    /// The peer rejected a block of the piece.
    pub fn on_rejected(&mut self, piece: ValidPieceIndex, now: Instant) {
        self.rejected.retain(|(p, _)| *p != piece);
        if self.rejected.len() >= MAX_REJECTED_REMEMBERED {
            self.rejected.pop_front();
        }
        self.rejected.push_back((piece, now));
    }

    /// The pieces not to ask the peer for right now, because it rejected them recently (see
    /// [`REJECTED_PIECE_BACKOFF`]).
    pub fn backed_off(&self, now: Instant) -> impl Iterator<Item = ValidPieceIndex> + '_ {
        self.rejected
            .iter()
            .filter(move |(_, at)| now.saturating_duration_since(*at) < REJECTED_PIECE_BACKOFF)
            .map(|(p, _)| *p)
    }

    /// The peer choked us while blocks were in flight, or we asked it for an allowed-fast block
    /// while choked: from now on it has [`choke_answer_timeout`] to answer. A clock already
    /// running is not restarted.
    pub fn expect_answers(&mut self, now: Instant) {
        self.choked_at.get_or_insert(now);
    }

    /// The peer answered a block with `Piece` or `RejectRequest`. A peer that keeps answering
    /// is not overdue: the clock restarts while blocks remain in flight, and stops once none do.
    pub fn on_answer(&mut self, now: Instant, more_in_flight: bool) {
        if self.choked_at.is_some() {
            self.choked_at = more_in_flight.then_some(now);
        }
    }

    /// The peer unchoked us. Its rejects are explained by the choke, so they are forgiven.
    pub fn on_unchoked(&mut self) {
        self.choked_at = None;
        self.rejected.clear();
    }

    /// Whether the peer, which chokes us, let `timeout` pass without answering what is in
    /// flight. Returns `true` once: the caller gives back every request in flight and every
    /// piece the peer owns, so nothing waits for an answer or a give-back any more.
    pub fn take_overdue(&mut self, now: Instant, timeout: Duration) -> bool {
        match self.choked_at {
            Some(at) if now.saturating_duration_since(at) >= timeout => {
                self.choked_at = None;
                self.give_back.clear();
                true
            }
            _ => false,
        }
    }

    /// The piece goes back to the queue once the peer answered the last block of it in flight.
    pub fn defer_give_back(&mut self, piece: ValidPieceIndex) {
        if !self.give_back.contains(&piece) {
            self.give_back.push(piece);
        }
    }

    /// Whether the piece was waiting to be given back; it is not any more.
    pub fn take_give_back(&mut self, piece: ValidPieceIndex) -> bool {
        match self.give_back.iter().position(|p| *p == piece) {
            Some(i) => {
                self.give_back.swap_remove(i);
                true
            }
            None => false,
        }
    }

    /// Whether the piece waits to be given back: no more of it is to be requested.
    pub fn gives_back(&self, piece: ValidPieceIndex) -> bool {
        self.give_back.contains(&piece)
    }

    /// The pieces that wait to be given back.
    pub fn giving_back(&self) -> impl Iterator<Item = ValidPieceIndex> + '_ {
        self.give_back.iter().copied()
    }
}

/// The uploading side of one connection with the Fast Extension: the allowed-fast set we
/// offered the peer, and which of its blocks we accepted while the peer was choked.
#[derive(Debug, Default)]
pub(crate) struct FastUpload {
    // `None` until we decided whether to offer a set (once, when the peer's piece set arrived).
    offered: Option<Vec<u32>>,
    // Blocks of offered pieces accepted while the peer was choked. Bounded by the blocks of at
    // most `ALLOWED_FAST_K` pieces: the caller only passes validated blocks.
    served: HashSet<BlockKey>,
}

impl FastUpload {
    /// Whether the offer was decided already.
    pub fn decided(&self) -> bool {
        self.offered.is_some()
    }

    /// Records what we offered the peer (possibly nothing). Decided once.
    pub fn set_offered(&mut self, pieces: Vec<u32>) {
        if self.offered.is_none() {
            self.offered = Some(pieces);
        }
    }

    /// Whether the piece is in the set we offered.
    pub fn is_offered(&self, piece: u32) -> bool {
        self.offered.as_ref().is_some_and(|o| o.contains(&piece))
    }

    /// A choked peer asks for a block of an offered piece: `true` the first time, so it is
    /// served past the choke once and not again.
    pub fn take_block(&mut self, key: BlockKey) -> bool {
        self.is_offered(key.0) && self.served.insert(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use librqbit_core::lengths::Lengths;

    fn piece(i: u32) -> ValidPieceIndex {
        // 100 pieces of 16 KiB.
        Lengths::new(100 * 16384, 16384)
            .unwrap()
            .validate_piece_index(i)
            .unwrap()
    }

    /// The worked example in BEP 6.
    #[test]
    fn test_allowed_fast_set_bep_example() {
        let ip: Ipv4Addr = "80.4.4.200".parse().unwrap();
        let info_hash = Id20::new([0xaa; 20]);
        let seven = allowed_fast_set(ip, &info_hash, 1313, 7);
        assert_eq!(seven, vec![1059, 431, 808, 1217, 287, 376, 1188]);
        let nine = allowed_fast_set(ip, &info_hash, 1313, 9);
        assert_eq!(nine, vec![1059, 431, 808, 1217, 287, 376, 1188, 353, 508]);
        // A larger k extends the smaller one; it never reshuffles it.
        assert_eq!(&nine[..7], seven.as_slice());
    }

    #[test]
    fn test_allowed_fast_set_masks_the_host_octet() {
        let info_hash = Id20::new([0xaa; 20]);
        let a = allowed_fast_set("80.4.4.200".parse().unwrap(), &info_hash, 1313, 7);
        let b = allowed_fast_set("80.4.4.1".parse().unwrap(), &info_hash, 1313, 7);
        let c = allowed_fast_set("80.4.5.200".parse().unwrap(), &info_hash, 1313, 7);
        assert_eq!(a, b, "addresses in the same /24 share a set");
        assert_ne!(a, c);
    }

    #[test]
    fn test_allowed_fast_set_stays_in_range_and_unique() {
        let set = allowed_fast_set("1.2.3.4".parse().unwrap(), &Id20::new([9; 20]), 3, 3);
        assert_eq!(set.len(), 3);
        let mut sorted = set.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, vec![0, 1, 2]);
    }

    #[test]
    fn test_allowed_fast_set_empty_inputs() {
        let ip: Ipv4Addr = "1.2.3.4".parse().unwrap();
        assert!(allowed_fast_set(ip, &Id20::new([0; 20]), 0, 10).is_empty());
        assert!(allowed_fast_set(ip, &Id20::new([0; 20]), 10, 0).is_empty());
        // Fewer pieces than k: every piece, and the loop ends.
        let mut small = allowed_fast_set(ip, &Id20::new([0; 20]), 2, 10);
        small.sort_unstable();
        assert_eq!(small, vec![0, 1]);
    }

    #[test]
    fn test_allowed_fast_received_is_bounded_and_unique() {
        let mut d = FastDownload::default();
        assert!(d.add_allowed_fast(piece(3)));
        assert!(!d.add_allowed_fast(piece(3)), "a repeat is not new");
        for i in 0..100 {
            d.add_allowed_fast(piece(i));
        }
        assert_eq!(d.allowed_fast().len(), MAX_ALLOWED_FAST_REMEMBERED);
        assert_eq!(d.allowed_fast()[0], piece(3), "arrival order is kept");
        assert!(d.is_allowed_fast(piece(3)));
        assert!(!d.is_allowed_fast(piece(99)));
    }

    #[test]
    fn test_suggestions_most_recent_first_and_bounded() {
        let mut d = FastDownload::default();
        d.add_suggestion(piece(1));
        d.add_suggestion(piece(2));
        d.add_suggestion(piece(1));
        assert_eq!(
            d.suggestions().collect::<Vec<_>>(),
            vec![piece(1), piece(2)]
        );
        for i in 0..50 {
            d.add_suggestion(piece(i));
        }
        let s: Vec<_> = d.suggestions().collect();
        assert_eq!(s.len(), MAX_SUGGESTIONS);
        assert_eq!(s[0], piece(49));
    }

    #[test]
    fn test_rejected_pieces_back_off_until_unchoke_or_timeout() {
        let mut d = FastDownload::default();
        let t0 = Instant::now();
        d.on_rejected(piece(5), t0);
        assert_eq!(d.backed_off(t0).collect::<Vec<_>>(), vec![piece(5)]);
        assert_eq!(d.backed_off(t0 + REJECTED_PIECE_BACKOFF).count(), 0);
        d.on_unchoked();
        assert_eq!(d.backed_off(t0).count(), 0, "an unchoke forgives rejects");
        for i in 0..100 {
            d.on_rejected(piece(i), t0);
        }
        let backed: Vec<_> = d.backed_off(t0).collect();
        assert_eq!(backed.len(), MAX_REJECTED_REMEMBERED);
        assert!(!backed.contains(&piece(0)), "the oldest is forgotten first");
    }

    #[test]
    fn test_answers_overdue_once_per_choke() {
        let mut d = FastDownload::default();
        let t0 = Instant::now();
        let timeout = Duration::from_secs(5);
        assert!(!d.take_overdue(t0 + timeout, timeout), "nothing expected");
        d.expect_answers(t0);
        assert!(!d.take_overdue(t0 + timeout / 2, timeout));
        assert!(d.take_overdue(t0 + timeout, timeout));
        assert!(!d.take_overdue(t0 + timeout * 2, timeout), "once per choke");
        d.expect_answers(t0);
        d.on_unchoked();
        assert!(
            !d.take_overdue(t0 + timeout, timeout),
            "an unchoke ends the wait"
        );
    }

    #[test]
    fn test_answers_restart_the_clock() {
        let mut d = FastDownload::default();
        let t0 = Instant::now();
        let timeout = Duration::from_secs(5);
        d.expect_answers(t0);
        d.expect_answers(t0 + timeout / 2);
        assert!(
            d.take_overdue(t0 + timeout, timeout),
            "a second expectation does not restart the clock"
        );
        d.expect_answers(t0);
        d.on_answer(t0 + timeout / 2, true);
        assert!(
            !d.take_overdue(t0 + timeout, timeout),
            "the peer is answering"
        );
        assert!(d.take_overdue(t0 + timeout / 2 + timeout, timeout));
        d.expect_answers(t0);
        d.on_answer(t0, false);
        assert!(
            !d.take_overdue(t0 + timeout * 2, timeout),
            "nothing is in flight any more"
        );
        d.on_answer(t0, true);
        assert!(
            !d.take_overdue(t0 + timeout * 2, timeout),
            "an answer while nothing was expected starts no clock"
        );
    }

    #[test]
    fn test_give_back_is_taken_once() {
        let mut d = FastDownload::default();
        d.defer_give_back(piece(7));
        d.defer_give_back(piece(7));
        assert!(d.take_give_back(piece(7)));
        assert!(!d.take_give_back(piece(7)));
        assert!(!d.take_give_back(piece(8)));
    }

    #[test]
    fn test_offered_blocks_are_served_once_while_choked() {
        let mut u = FastUpload::default();
        assert!(!u.decided());
        assert!(!u.take_block((1, 0, 16384)), "nothing offered yet");
        u.set_offered(vec![1, 2]);
        u.set_offered(vec![3]);
        assert!(u.decided());
        assert!(
            u.is_offered(1) && u.is_offered(2) && !u.is_offered(3),
            "decided once"
        );
        assert!(u.take_block((1, 0, 16384)));
        assert!(
            !u.take_block((1, 0, 16384)),
            "the same block is not served twice"
        );
        assert!(u.take_block((1, 16384, 16384)));
        assert!(!u.take_block((3, 0, 16384)));
    }
}
