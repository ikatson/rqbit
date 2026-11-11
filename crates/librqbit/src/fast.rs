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

/// After we choke a peer, every request we turn down on arrival is rejected for this long,
/// however many: the peer sent them before the choke reached it, up to `reqq` of them, and a
/// fast peer far away has many in transit.
pub(crate) const CHOKE_REJECT_GRACE: Duration = Duration::from_secs(2);

/// Requests we turn down on arrival (the peer is choked, its queue is full, or we do not have
/// the piece) that are rejected between two chokes, unchokes or served blocks, outside of the
/// [`CHOKE_REJECT_GRACE`]. The advertised `reqq`: a peer that keeps no more requests in flight
/// than we allow never gets there. Past it the connection is closed. BEP 6 wants every request
/// answered, so dropping them silently is not an option, and answering without end would let
/// a peer that does not read grow our write queue without bound.
pub(crate) const MAX_REJECTS_PER_CHOKE: u32 = crate::upload_gate::MAX_QUEUED_UPLOADS_PER_PEER;

/// Allowed-fast indexes remembered from one peer. *k* is 10; the cap only bounds a peer that
/// sends the message without end.
pub(crate) const MAX_ALLOWED_FAST_REMEMBERED: usize = 32;

/// Suggest-piece indexes remembered from one peer, most recent first.
pub(crate) const MAX_SUGGESTIONS: usize = 16;

/// A piece a peer rejected is not asked of that peer again for this long, unless the peer
/// unchokes us in between (a choke explains the rejects). Without it a peer that rejects while
/// unchoked would get the same request again right away, in a tight loop.
pub(crate) const REJECTED_PIECE_BACKOFF: Duration = Duration::from_secs(30);

/// Rejected pieces remembered per peer for [`REJECTED_PIECE_BACKOFF`]. When a reject arrives
/// while the list is full the oldest entry is forgotten, and nothing more is asked of the peer
/// until that entry would have expired: forgetting it must not make its piece requestable
/// right away.
pub(crate) const MAX_REJECTED_REMEMBERED: usize = 64;

/// Rejects in a row, received while the peer unchokes us and with no block in between, after
/// which nothing more is asked of the peer for [`REJECTED_PIECE_BACKOFF`]. The per-piece
/// back-off alone does not stop a peer that rejects everything: there is always another piece
/// to try. 16 lets a peer turn down a few pieces it cannot serve (a partial seed whose bitfield
/// we read wrong, a full queue) without losing it, and stops the loop within a request window.
pub(crate) const REJECT_STREAK_PAUSE: u32 = 16;

/// Rejects in a row, received while the peer unchokes us and with no block in between, after
/// which the peer is disconnected: it unchokes us and serves nothing, so it only takes a
/// connection slot. Two full request windows (see
/// [`PeerConnectionOptions::max_request_window`](crate::PeerConnectionOptions)), so the
/// requests already on their way when the pause starts do not count twice.
pub(crate) const MAX_REJECT_STREAK: u32 = 256;

/// How long a peer that choked us has to answer the requests it voided before their pieces are
/// given back to the queue, as they are right away without the extension. Answered means
/// `Piece` or `RejectRequest`; each answer for such a piece restarts the clock.
///
/// Half the read timeout: the reader asks [`may_be_silent`] after every half read timeout of
/// silence, and a silent peer is given up on after a full one. So by the first question after a
/// choke followed by silence the requests are overdue: they are given back, and the silence is
/// excused, before the reader would give up on the peer. A peer that keeps talking without
/// answering is caught after its next message.
///
/// Allowed-fast requests are not covered: the peer serves them while it chokes us, their
/// answer is the payload, and a slow uploader may take longer. The silence timeout and the
/// steal of slow pieces cover those.
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
    // Rejects received while the peer unchoked us since the last block it sent.
    reject_streak: u32,
    // Nothing is asked of the peer before this (see `REJECT_STREAK_PAUSE`).
    paused_until: Option<Instant>,
    // Pieces we own that go back to the queue once the last block of them in flight to the
    // peer is answered: the peer choked us, or rejected another block of them.
    give_back: Vec<ValidPieceIndex>,
    // Since when the peer owes us answers for the blocks of `give_back`: set while it is not
    // empty, restarted by every answer for one of its pieces.
    answers_due_since: Option<Instant>,
    // Download-rate budget paid for requests the peer rejected, spent on the next requests to
    // the peer before the rate limiter is asked (the limiter cannot take tokens back).
    rate_credit: u32,
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

    /// The peer rejected a block of an allowed-fast piece while it chokes us: it will not serve
    /// the piece past the choke any more (it served it once already, or we are no longer a
    /// newcomer to it), so asking again would only collect more rejects.
    pub fn remove_allowed_fast(&mut self, piece: ValidPieceIndex) {
        self.allowed_fast.retain(|p| *p != piece);
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

    /// The peer rejected a block of the piece. `unchoked`: the peer unchokes us, so the reject
    /// is not explained by a choke and counts towards the streak. Returns `true` when the peer
    /// rejected [`MAX_REJECT_STREAK`] requests in a row without sending a block: it is useless.
    pub fn on_rejected(&mut self, piece: ValidPieceIndex, now: Instant, unchoked: bool) -> bool {
        while self
            .rejected
            .front()
            .is_some_and(|(_, at)| now.saturating_duration_since(*at) >= REJECTED_PIECE_BACKOFF)
        {
            self.rejected.pop_front();
        }
        self.rejected.retain(|(p, _)| *p != piece);
        if self.rejected.len() >= MAX_REJECTED_REMEMBERED
            && let Some((_, oldest)) = self.rejected.pop_front()
            && unchoked
        {
            self.pause_until(oldest + REJECTED_PIECE_BACKOFF);
        }
        self.rejected.push_back((piece, now));
        self.on_answer(piece, now);
        if !unchoked {
            return false;
        }
        self.reject_streak = self.reject_streak.saturating_add(1);
        if self.reject_streak.is_multiple_of(REJECT_STREAK_PAUSE) {
            self.pause_until(now + REJECTED_PIECE_BACKOFF);
        }
        self.reject_streak >= MAX_REJECT_STREAK
    }

    /// Gives back the download-rate budget paid for a request the peer rejected: it pays for
    /// the next requests to the peer. Without it a peer that rejects everything would drain the
    /// rate limit shared with the other peers.
    pub fn refund_rate(&mut self, len: u32) {
        self.rate_credit = self.rate_credit.saturating_add(len);
    }

    /// Takes up to `len` bytes of refunded budget. Returns what is left to pay for.
    pub fn spend_rate_credit(&mut self, len: u32) -> u32 {
        let spent = self.rate_credit.min(len);
        self.rate_credit -= spent;
        len - spent
    }

    /// The peer sent a block of the piece: it serves, the reject streak is over.
    pub fn on_block(&mut self, piece: ValidPieceIndex, now: Instant) {
        self.reject_streak = 0;
        self.on_answer(piece, now);
    }

    fn pause_until(&mut self, until: Instant) {
        self.paused_until = Some(self.paused_until.map_or(until, |p| p.max(until)));
    }

    /// Whether nothing is to be asked of the peer right now, because it rejected too much (see
    /// [`REJECT_STREAK_PAUSE`] and [`MAX_REJECTED_REMEMBERED`]).
    pub fn is_paused(&self, now: Instant) -> bool {
        self.paused_until.is_some_and(|until| now < until)
    }

    /// The pieces not to ask the peer for right now, because it rejected them recently (see
    /// [`REJECTED_PIECE_BACKOFF`]).
    pub fn backed_off(&self, now: Instant) -> impl Iterator<Item = ValidPieceIndex> + '_ {
        self.rejected
            .iter()
            .filter(move |(_, at)| now.saturating_duration_since(*at) < REJECTED_PIECE_BACKOFF)
            .map(|(p, _)| *p)
    }

    /// The peer unchoked us after choking us (a redundant `Unchoke` must not get here). Its
    /// rejects are explained by the choke, so they are forgiven, and so is the pause; the
    /// streak is not, or a peer could reset it with a choke and an unchoke. The pieces that
    /// wait to be given back still wait, and their clock keeps running: the requests the choke
    /// voided are owed an answer all the same.
    pub fn on_unchoked(&mut self) {
        self.rejected.clear();
        self.paused_until = None;
    }

    /// An answer for a block of the piece arrived: when the piece waits to be given back, the
    /// peer is answering, and the clock restarts.
    fn on_answer(&mut self, piece: ValidPieceIndex, now: Instant) {
        if self.answers_due_since.is_some() && self.give_back.contains(&piece) {
            self.answers_due_since = Some(now);
        }
    }

    /// Whether the peer let `timeout` pass without answering the requests of the pieces that
    /// wait to be given back. Returns those pieces, once: the caller cancels their requests and
    /// gives them back. Empty when nothing is overdue.
    pub fn take_overdue(&mut self, now: Instant, timeout: Duration) -> Vec<ValidPieceIndex> {
        match self.answers_due_since {
            Some(at) if now.saturating_duration_since(at) >= timeout => {
                self.answers_due_since = None;
                std::mem::take(&mut self.give_back)
            }
            _ => Vec::new(),
        }
    }

    /// The piece goes back to the queue once the peer answered the last block of it in flight,
    /// or when it let [`choke_answer_timeout`] pass without answering. A clock already running
    /// is not restarted.
    pub fn defer_give_back(&mut self, piece: ValidPieceIndex, now: Instant) {
        if !self.give_back.contains(&piece) {
            self.give_back.push(piece);
        }
        self.answers_due_since.get_or_insert(now);
    }

    /// Whether the piece was waiting to be given back; it is not any more.
    pub fn take_give_back(&mut self, piece: ValidPieceIndex) -> bool {
        let taken = match self.give_back.iter().position(|p| *p == piece) {
            Some(i) => {
                self.give_back.swap_remove(i);
                true
            }
            None => false,
        };
        self.stop_clock_when_done();
        taken
    }

    /// Keeps only the pieces that still wait to be given back. A piece another peer stole
    /// meanwhile is not ours to give back: our requests for it were cancelled, so their answers
    /// never reach [`take_give_back`](Self::take_give_back).
    pub fn retain_give_back(&mut self, mut keep: impl FnMut(ValidPieceIndex) -> bool) {
        self.give_back.retain(|p| keep(*p));
        self.stop_clock_when_done();
    }

    fn stop_clock_when_done(&mut self) {
        if self.give_back.is_empty() {
            self.answers_due_since = None;
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
        assert!(!d.on_rejected(piece(5), t0, false));
        assert_eq!(d.backed_off(t0).collect::<Vec<_>>(), vec![piece(5)]);
        assert_eq!(d.backed_off(t0 + REJECTED_PIECE_BACKOFF).count(), 0);
        d.on_unchoked();
        assert_eq!(d.backed_off(t0).count(), 0, "an unchoke forgives rejects");
        for i in 0..100 {
            d.on_rejected(piece(i), t0, false);
        }
        let backed: Vec<_> = d.backed_off(t0).collect();
        assert_eq!(backed.len(), MAX_REJECTED_REMEMBERED);
        assert!(!backed.contains(&piece(0)), "the oldest is forgotten first");
        assert!(
            !d.is_paused(t0),
            "rejects while choked are explained by the choke"
        );
    }

    // Scenario A of the review: a peer that unchokes us and rejects everything. The streak
    // pauses requests, and a peer that goes on is given up on.
    #[test]
    fn test_reject_streak_pauses_then_gives_up() {
        let mut d = FastDownload::default();
        let t0 = Instant::now();
        for i in 0..REJECT_STREAK_PAUSE - 1 {
            assert!(!d.on_rejected(piece(i), t0, true));
        }
        assert!(!d.is_paused(t0));
        assert!(!d.on_rejected(piece(99), t0, true));
        assert!(d.is_paused(t0));
        assert!(d.is_paused(t0 + REJECTED_PIECE_BACKOFF - Duration::from_millis(1)));
        assert!(!d.is_paused(t0 + REJECTED_PIECE_BACKOFF));

        // A block ends the streak.
        d.on_block(piece(98), t0);
        for i in 0..REJECT_STREAK_PAUSE - 1 {
            d.on_rejected(piece(i), t0 + REJECTED_PIECE_BACKOFF, true);
        }
        assert!(!d.is_paused(t0 + REJECTED_PIECE_BACKOFF));

        let mut d = FastDownload::default();
        let mut useless = false;
        for i in 0..MAX_REJECT_STREAK {
            useless = d.on_rejected(piece(i % 90), t0, true);
            assert_eq!(useless, i + 1 == MAX_REJECT_STREAK);
        }
        assert!(useless);
    }

    // A full list must not make its oldest piece requestable right away.
    #[test]
    fn test_full_reject_list_pauses_until_the_oldest_expires() {
        let mut d = FastDownload::default();
        let t0 = Instant::now();
        let step = Duration::from_millis(10);
        let mut now = t0;
        for i in 0..MAX_REJECTED_REMEMBERED as u32 {
            d.on_rejected(piece(i), now, true);
            // Blocks in between keep the streak short: only the list is full.
            d.on_block(piece(99), now);
            now += step;
        }
        assert!(!d.is_paused(now));
        d.on_rejected(piece(98), now, true);
        assert!(d.is_paused(t0 + REJECTED_PIECE_BACKOFF - step));
        assert!(!d.is_paused(t0 + REJECTED_PIECE_BACKOFF));
    }

    // Scenario B of the review: redundant unchokes do not forgive anything, because only a
    // choked-to-unchoked transition calls `on_unchoked`; a real one forgives the back-off and
    // the pause but not the streak.
    #[test]
    fn test_unchoke_forgives_back_off_but_not_the_streak() {
        let mut d = FastDownload::default();
        let t0 = Instant::now();
        for i in 0..REJECT_STREAK_PAUSE {
            d.on_rejected(piece(i), t0, true);
        }
        assert!(d.is_paused(t0));
        d.on_unchoked();
        assert!(!d.is_paused(t0));
        assert_eq!(d.backed_off(t0).count(), 0);
        let mut useless = false;
        for i in REJECT_STREAK_PAUSE..MAX_REJECT_STREAK {
            useless = d.on_rejected(piece(i % 90), t0, true);
        }
        assert!(useless, "the streak survived the unchoke");
    }

    #[test]
    fn test_rejected_allowed_fast_piece_is_forgotten() {
        let mut d = FastDownload::default();
        d.add_allowed_fast(piece(1));
        d.add_allowed_fast(piece(2));
        d.remove_allowed_fast(piece(1));
        assert_eq!(d.allowed_fast(), &[piece(2)]);
    }

    #[test]
    fn test_overdue_only_while_pieces_wait_to_be_given_back() {
        let mut d = FastDownload::default();
        let t0 = Instant::now();
        let timeout = Duration::from_secs(5);
        assert!(
            d.take_overdue(t0 + timeout, timeout).is_empty(),
            "nothing owed"
        );
        d.defer_give_back(piece(1), t0);
        d.defer_give_back(piece(2), t0 + timeout / 2);
        assert!(d.take_overdue(t0 + timeout / 2, timeout).is_empty());
        let mut overdue = d.take_overdue(t0 + timeout, timeout);
        overdue.sort_by_key(|p| p.get());
        assert_eq!(
            overdue,
            vec![piece(1), piece(2)],
            "a second piece does not restart the clock"
        );
        assert!(
            d.take_overdue(t0 + timeout * 2, timeout).is_empty(),
            "taken once"
        );
        assert_eq!(d.giving_back().count(), 0);
    }

    #[test]
    fn test_answers_for_deferred_pieces_restart_the_clock() {
        let mut d = FastDownload::default();
        let t0 = Instant::now();
        let timeout = Duration::from_secs(5);
        d.defer_give_back(piece(1), t0);
        // An answer for another piece (an allowed-fast block) does not count.
        d.on_block(piece(9), t0 + timeout / 2);
        assert_eq!(d.take_overdue(t0 + timeout, timeout), vec![piece(1)]);

        d.defer_give_back(piece(1), t0);
        d.on_block(piece(1), t0 + timeout / 2);
        assert!(
            d.take_overdue(t0 + timeout, timeout).is_empty(),
            "the peer is answering"
        );
        d.on_rejected(piece(1), t0 + timeout, false);
        assert!(
            d.take_overdue(t0 + timeout + timeout / 2, timeout)
                .is_empty()
        );
        assert_eq!(
            d.take_overdue(t0 + timeout * 2, timeout),
            vec![piece(1)],
            "a reject is an answer too"
        );
    }

    // Finding 3 of the review: neither an unchoke nor a steal leaves the clock stopped, or a
    // stale entry behind.
    #[test]
    fn test_unchoke_keeps_the_clock_and_stolen_pieces_are_dropped() {
        let mut d = FastDownload::default();
        let t0 = Instant::now();
        let timeout = Duration::from_secs(5);
        d.defer_give_back(piece(1), t0);
        d.defer_give_back(piece(2), t0);
        d.on_unchoked();
        assert_eq!(d.take_overdue(t0 + timeout, timeout).len(), 2);

        d.defer_give_back(piece(1), t0);
        d.defer_give_back(piece(2), t0);
        d.retain_give_back(|p| p != piece(2));
        assert!(d.gives_back(piece(1)) && !d.gives_back(piece(2)));
        assert!(d.take_give_back(piece(1)));
        assert!(!d.take_give_back(piece(1)), "taken once");
        assert!(
            d.take_overdue(t0 + timeout, timeout).is_empty(),
            "nothing left: the clock stopped"
        );
        d.defer_give_back(piece(3), t0);
        d.retain_give_back(|_| false);
        assert!(d.take_overdue(t0 + timeout, timeout).is_empty());
    }

    #[test]
    fn test_rejected_requests_refund_the_rate_budget() {
        let mut d = FastDownload::default();
        assert_eq!(d.spend_rate_credit(16384), 16384, "nothing to spend yet");
        d.refund_rate(16384);
        d.refund_rate(8192);
        assert_eq!(d.spend_rate_credit(16384), 0, "paid by the refund");
        assert_eq!(d.spend_rate_credit(16384), 8192, "what is left of it");
        assert_eq!(d.spend_rate_credit(16384), 16384, "spent once");
        d.refund_rate(u32::MAX);
        d.refund_rate(1);
        assert_eq!(d.spend_rate_credit(u32::MAX), 0, "saturates");
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
