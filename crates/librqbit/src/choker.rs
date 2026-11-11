//! Choking: which peers a torrent uploads to.
//!
//! Uploading to every peer that asks spreads the upload rate thin. Nobody gets a useful rate out
//! of us, so the peers that pick whom they upload to by what they receive (tit for tat) drop us,
//! and a piece takes long to reach anybody in full. So, like other clients (BEP 3, libtorrent), a
//! torrent uploads to a limited number of interested peers at a time, its *upload slots*. A peer
//! that holds a slot is unchoked, every other peer is choked.
//!
//! * Regular slots go to the peers we download the fastest from, then to the ones we upload the
//!   fastest to (the only thing left to compare when seeding). They are handed out anew every
//!   [`RECHOKE_INTERVAL`].
//! * One slot in five, and at least one, is optimistic: every [`OPTIMISTIC_INTERVAL`] it moves to
//!   the peer that went the longest without a slot, whatever its rates. This is how a peer that
//!   has nothing to offer yet gets its first pieces, and how better peers than the current ones
//!   are found.
//! * A free slot is given away at once: a peer becomes interested while there is one, or the
//!   holder disconnects or loses interest. A slot is only taken from an interested peer at the
//!   interval, and not before the peer had [`SLOT_GRACE`] to use it, so that it lasts long enough
//!   to be worth something.

use std::{
    cmp::Reverse,
    collections::HashMap,
    num::NonZeroU32,
    sync::Arc,
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use tracing::trace;

/// How often the upload slots are handed out anew.
pub(crate) const RECHOKE_INTERVAL: Duration = Duration::from_secs(10);
/// How often the optimistic slots move on to other peers.
const OPTIMISTIC_INTERVAL: Duration = Duration::from_secs(30);
/// A slot given less than this long ago is not up for a rechoke yet: its peer had no time to
/// make use of it.
const SLOT_GRACE: Duration = Duration::from_secs(5);

/// [`UploadSlots::RateBased`] never has fewer slots than a classic client.
const MIN_RATE_BASED_SLOTS: usize = 4;
/// [`UploadSlots::RateBased`]: the upload rate (bytes per second) that makes the fastest peer
/// worth a slot, and by how much the bar is raised for every next peer. The values of
/// libtorrent's rate based choker.
const RATE_BASED_THRESHOLD: u64 = 1024;
const RATE_BASED_THRESHOLD_STEP: u64 = 2048;

/// How many peers a torrent uploads to at a time. The other interested peers are choked and
/// wait for their turn.
///
/// Set for the session with [`SessionOptions::upload_slots`](crate::SessionOptions::upload_slots);
/// every torrent has that many slots of its own. As text it is `auto` (rate based), a number
/// or `unlimited`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UploadSlots {
    /// As many peers as take data at a worthwhile rate, and at least 4.
    ///
    /// Peers are counted fastest first. One is worth a slot when we upload to it faster than
    /// 1 KiB/s plus 2 KiB/s for every peer counted before it, and there is one slot more than
    /// such peers. So the slots grow for as long as the upload rate keeps up with them (about
    /// 22 at 1 MiB/s, 50 at 5 MiB/s) and shrink when it gets spread too thin. This is
    /// libtorrent's rate based choker.
    #[default]
    RateBased,
    /// This many peers.
    Fixed(NonZeroU32),
    /// Every interested peer: nobody waits.
    Unlimited,
}

impl std::fmt::Display for UploadSlots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateBased => f.write_str("auto"),
            Self::Fixed(n) => write!(f, "{n}"),
            Self::Unlimited => f.write_str("unlimited"),
        }
    }
}

impl std::str::FromStr for UploadSlots {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(Self::RateBased),
            "unlimited" => Ok(Self::Unlimited),
            n => n.parse().map(Self::Fixed).map_err(|_| {
                anyhow::anyhow!("expected a positive number, \"auto\" or \"unlimited\", got {s:?}")
            }),
        }
    }
}

/// How many bytes went each way on a connection. Only the growth matters.
#[derive(Clone, Copy, Default)]
pub(crate) struct Transferred {
    /// Received from the peer.
    pub down: u64,
    /// Sent to the peer.
    pub up: u64,
}

/// One peer connection, as the choker sees it.
pub(crate) trait ChokerPeer: Send + Sync {
    /// Tells the peer it is choked or unchoked, and makes it so on our side: what a choked peer
    /// had queued is dropped, what it asks for is ignored.
    fn set_choked(&self, choked: bool);

    fn transferred(&self) -> Transferred;
}

/// What the peer policy made of a connection, as far as the upload slots go.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum SlotClass {
    /// It competes for the slots.
    #[default]
    Contender,
    /// It is unchoked whenever it is interested, and takes no slot: what it gets is limited
    /// elsewhere (the throttled upload queue).
    Exempt,
    /// It stays choked.
    Barred,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SlotKind {
    Regular,
    Optimistic,
}

#[derive(Clone, Copy)]
struct Slot {
    kind: SlotKind,
    since: Instant,
}

struct Entry {
    peer: Arc<dyn ChokerPeer>,
    since: Instant,
    interested: bool,
    class: SlotClass,
    // Only an interested contender holds a slot.
    slot: Option<Slot>,
    // When it lost its last slot. `None`: it never had one.
    choked_at: Option<Instant>,
    // The counters at the last round, and the rates (bytes per second) of that round.
    mark: Transferred,
    down_rate: u64,
    up_rate: u64,
}

// Sorts the better peer first: the one we download from the fastest, then the one we upload to
// the fastest, then the one that went the longest without a slot.
type Rank = (Reverse<u64>, Reverse<u64>, Option<Instant>, u64);

impl Entry {
    fn holds(&self, kind: SlotKind) -> bool {
        self.slot.is_some_and(|s| s.kind == kind)
    }

    /// It wants a slot and may have one.
    fn contends(&self) -> bool {
        self.interested && self.class == SlotClass::Contender
    }

    fn unchoked(&self) -> bool {
        self.slot.is_some() || (self.interested && self.class == SlotClass::Exempt)
    }

    /// The last moment it had a slot.
    fn last_served(&self, now: Instant) -> Option<Instant> {
        if self.slot.is_some() {
            Some(now)
        } else {
            self.choked_at
        }
    }

    fn rank(&self, id: u64, now: Instant) -> Rank {
        (
            Reverse(self.down_rate),
            Reverse(self.up_rate),
            self.last_served(now),
            id,
        )
    }
}

fn rate(bytes: u64, window: Duration) -> u64 {
    let ms = window.as_millis().max(1000);
    u64::try_from(u128::from(bytes) * 1000 / ms).unwrap_or(u64::MAX)
}

/// How many of the slots are optimistic.
fn optimistic_share(slots: usize) -> usize {
    if slots == usize::MAX {
        return 0;
    }
    (slots / 5).max(1).min(slots)
}

struct Inner {
    mode: UploadSlots,
    // The slots of the current round.
    slots: usize,
    peers: HashMap<u64, Entry>,
    next_id: u64,
    rechoked_at: Instant,
    rotated_at: Instant,
}

impl Inner {
    fn give(&mut self, id: u64, kind: SlotKind, now: Instant) {
        let Some(e) = self.peers.get_mut(&id) else {
            return;
        };
        match &mut e.slot {
            Some(slot) => slot.kind = kind,
            None => {
                e.slot = Some(Slot { kind, since: now });
                e.peer.set_choked(false);
            }
        }
    }

    fn take(&mut self, id: u64, now: Instant) {
        let Some(e) = self.peers.get_mut(&id) else {
            return;
        };
        if e.slot.take().is_some() {
            e.choked_at = Some(now);
            e.peer.set_choked(true);
        }
    }

    /// Gives the slots nobody holds to the peers that wait.
    fn fill(&mut self, now: Instant) {
        let optimistic_share = optimistic_share(self.slots);
        let regular_share = self.slots - optimistic_share;
        loop {
            let held = |kind| self.peers.values().filter(|e| e.holds(kind)).count();
            let kind = if held(SlotKind::Regular) < regular_share {
                SlotKind::Regular
            } else if held(SlotKind::Optimistic) < optimistic_share {
                SlotKind::Optimistic
            } else {
                return;
            };
            let waiting = self
                .peers
                .iter()
                .filter(|(_, e)| e.contends() && e.slot.is_none());
            let next = match kind {
                SlotKind::Regular => waiting.min_by_key(|(id, e)| e.rank(**id, now)),
                SlotKind::Optimistic => waiting.min_by_key(|(id, e)| (e.choked_at, **id)),
            };
            let Some((&id, _)) = next else {
                return;
            };
            self.give(id, kind, now);
        }
    }

    /// The rates of the round that ends now.
    fn measure(&mut self, now: Instant) {
        let round_start = self.rechoked_at;
        for e in self.peers.values_mut() {
            let total = e.peer.transferred();
            e.down_rate = rate(
                total.down.saturating_sub(e.mark.down),
                now.saturating_duration_since(round_start.max(e.since)),
            );
            // A choked peer may still get what was on its way when it lost the slot. That
            // must not put it ahead of the peers that never had one.
            e.up_rate = e.slot.map_or(0, |slot| {
                rate(
                    total.up.saturating_sub(e.mark.up),
                    now.saturating_duration_since(round_start.max(slot.since)),
                )
            });
            e.mark = total;
        }
        self.rechoked_at = now;
    }

    fn slot_count(&self) -> usize {
        match self.mode {
            UploadSlots::Fixed(n) => usize::try_from(n.get()).unwrap_or(usize::MAX),
            UploadSlots::Unlimited => usize::MAX,
            UploadSlots::RateBased => {
                let mut rates: Vec<u64> = self
                    .peers
                    .values()
                    .filter(|e| e.slot.is_some())
                    .map(|e| e.up_rate)
                    .collect();
                rates.sort_unstable_by_key(|r| Reverse(*r));
                let mut threshold = RATE_BASED_THRESHOLD;
                let mut worthwhile = 0;
                for r in rates {
                    if r < threshold {
                        break;
                    }
                    worthwhile += 1;
                    threshold += RATE_BASED_THRESHOLD_STEP;
                }
                (worthwhile + 1).max(MIN_RATE_BASED_SLOTS)
            }
        }
    }

    fn rechoke(&mut self, now: Instant) {
        self.measure(now);
        self.slots = self.slot_count();
        let mut optimistic_share = optimistic_share(self.slots);
        let mut regular_share = self.slots - optimistic_share;

        let mut line: Vec<(Rank, u64)> = Vec::new();
        for (id, e) in self.peers.iter().filter(|(_, e)| e.contends()) {
            match e.slot {
                Some(slot) if now.saturating_duration_since(slot.since) < SLOT_GRACE => {
                    match slot.kind {
                        SlotKind::Regular => regular_share = regular_share.saturating_sub(1),
                        SlotKind::Optimistic => {
                            optimistic_share = optimistic_share.saturating_sub(1)
                        }
                    }
                }
                _ => line.push((e.rank(*id, now), *id)),
            }
        }
        line.sort_unstable();
        let regular_len = regular_share.min(line.len());

        // The others queue for the optimistic slots: whoever went the longest without a slot
        // comes first. Between two rotations the holders keep theirs.
        let rotate = now.saturating_duration_since(self.rotated_at) >= OPTIMISTIC_INTERVAL;
        if rotate {
            self.rotated_at = now;
        }
        let mut rest: Vec<(bool, Option<Instant>, u64)> = line[regular_len..]
            .iter()
            .filter_map(|(_, id)| {
                let e = self.peers.get(id)?;
                let keeps = !rotate && e.holds(SlotKind::Optimistic);
                Some((!keeps, e.last_served(now), *id))
            })
            .collect();
        rest.sort_unstable();

        for (_, id) in &line[..regular_len] {
            self.give(*id, SlotKind::Regular, now);
        }
        for (i, (_, _, id)) in rest.iter().enumerate() {
            if i < optimistic_share {
                self.give(*id, SlotKind::Optimistic, now);
            } else {
                self.take(*id, now);
            }
        }
        trace!(
            slots = self.slots,
            interested = line.len(),
            rotate,
            "upload slots handed out"
        );
    }
}

/// Hands out the upload slots of one torrent.
pub(crate) struct Choker {
    inner: Mutex<Inner>,
}

impl Choker {
    pub fn new(mode: UploadSlots, now: Instant) -> Self {
        let mut inner = Inner {
            mode,
            slots: 0,
            peers: HashMap::new(),
            next_id: 0,
            rechoked_at: now,
            rotated_at: now,
        };
        inner.slots = inner.slot_count();
        Self {
            inner: Mutex::new(inner),
        }
    }

    /// A new connection. It starts choked and not interested (BEP 3).
    pub fn add(self: &Arc<Self>, peer: Arc<dyn ChokerPeer>, now: Instant) -> Registration {
        let mut g = self.inner.lock();
        let id = g.next_id;
        g.next_id += 1;
        let mark = peer.transferred();
        g.peers.insert(
            id,
            Entry {
                peer,
                since: now,
                interested: false,
                class: SlotClass::default(),
                slot: None,
                choked_at: None,
                mark,
                down_rate: 0,
                up_rate: 0,
            },
        );
        Registration {
            choker: self.clone(),
            id,
        }
    }

    fn set_interested(&self, id: u64, interested: bool, now: Instant) {
        let mut g = self.inner.lock();
        let class = match g.peers.get_mut(&id) {
            Some(e) if e.interested != interested => {
                e.interested = interested;
                e.class
            }
            _ => return,
        };
        if class == SlotClass::Exempt {
            if let Some(e) = g.peers.get(&id) {
                e.peer.set_choked(!interested);
            }
        } else if !interested {
            g.take(id, now);
        }
        g.fill(now);
    }

    fn set_class(&self, id: u64, class: SlotClass, now: Instant) {
        let mut g = self.inner.lock();
        match g.peers.get_mut(&id) {
            Some(e) if e.class != class => {
                let was_unchoked = e.unchoked();
                e.class = class;
                // Whatever it becomes, it starts without a slot.
                if e.slot.take().is_some() {
                    e.choked_at = Some(now);
                }
                if e.unchoked() != was_unchoked {
                    e.peer.set_choked(was_unchoked);
                }
            }
            _ => return,
        }
        g.fill(now);
    }

    fn waits(&self, id: u64) -> bool {
        let g = self.inner.lock();
        g.peers
            .get(&id)
            .is_some_and(|e| e.contends() && e.slot.is_none())
    }

    fn remove(&self, id: u64, now: Instant) {
        let mut g = self.inner.lock();
        if g.peers.remove(&id).is_some_and(|e| e.slot.is_some()) {
            g.fill(now);
        }
    }

    /// Hands out the slots anew. To be called every [`RECHOKE_INTERVAL`].
    pub fn rechoke(&self, now: Instant) {
        self.inner.lock().rechoke(now);
    }
}

/// A connection known to the [`Choker`]. Dropping it gives its slot to the next peer.
pub(crate) struct Registration {
    choker: Arc<Choker>,
    id: u64,
}

impl Registration {
    /// The peer told us whether it wants our pieces.
    pub fn set_interested(&self, interested: bool, now: Instant) {
        self.choker.set_interested(self.id, interested, now);
    }

    /// The peer wants our pieces and has no slot: until it gets one it has nothing to ask for.
    pub fn waits_for_slot(&self) -> bool {
        self.choker.waits(self.id)
    }

    /// The peer policy decided how the connection is treated from now on.
    pub fn set_class(&self, class: SlotClass, now: Instant) {
        self.choker.set_class(self.id, class, now);
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.choker.remove(self.id, Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    const KIB: u64 = 1024;

    #[derive(Default)]
    struct FakePeer {
        // Every choke (true) and unchoke (false) it was sent.
        sent: Mutex<Vec<bool>>,
        down: AtomicU64,
        up: AtomicU64,
    }

    impl FakePeer {
        fn unchoked(&self) -> bool {
            self.sent.lock().last() == Some(&false)
        }
    }

    impl ChokerPeer for FakePeer {
        fn set_choked(&self, choked: bool) {
            self.sent.lock().push(choked);
        }

        fn transferred(&self) -> Transferred {
            Transferred {
                down: self.down.load(Ordering::Relaxed),
                up: self.up.load(Ordering::Relaxed),
            }
        }
    }

    struct Swarm {
        choker: Arc<Choker>,
        t0: Instant,
        peers: Vec<(Arc<FakePeer>, Option<Registration>)>,
    }

    impl Swarm {
        fn new(mode: UploadSlots) -> Self {
            let t0 = Instant::now();
            Self {
                choker: Arc::new(Choker::new(mode, t0)),
                t0,
                peers: Vec::new(),
            }
        }

        fn fixed(slots: u32) -> Self {
            Self::new(UploadSlots::Fixed(NonZeroU32::new(slots).unwrap()))
        }

        fn at(&self, secs: u64) -> Instant {
            self.t0 + Duration::from_secs(secs)
        }

        /// A peer connects at `secs`. Returns its index.
        fn connect(&mut self, secs: u64) -> usize {
            let peer = Arc::new(FakePeer::default());
            let reg = self.choker.add(peer.clone(), self.at(secs));
            self.peers.push((peer, Some(reg)));
            self.peers.len() - 1
        }

        /// `n` peers connect at `secs` and are interested right away, in order.
        fn join(&mut self, n: usize, secs: u64) {
            for _ in 0..n {
                let i = self.connect(secs);
                self.interested(i, true, secs);
            }
        }

        fn interested(&self, i: usize, interested: bool, secs: u64) {
            self.peers[i]
                .1
                .as_ref()
                .unwrap()
                .set_interested(interested, self.at(secs));
        }

        fn disconnect(&mut self, i: usize, secs: u64) {
            let reg = self.peers[i].1.take().unwrap();
            self.choker.remove(reg.id, self.at(secs));
        }

        fn uploaded(&self, i: usize, bytes: u64) {
            self.peers[i].0.up.fetch_add(bytes, Ordering::Relaxed);
        }

        fn downloaded(&self, i: usize, bytes: u64) {
            self.peers[i].0.down.fetch_add(bytes, Ordering::Relaxed);
        }

        fn rechoke(&self, secs: u64) {
            self.choker.rechoke(self.at(secs));
        }

        /// The peer policy's word on a peer.
        fn class(&self, i: usize, class: SlotClass, secs: u64) {
            self.peers[i]
                .1
                .as_ref()
                .unwrap()
                .set_class(class, self.at(secs));
        }

        /// The connected peers that were told they are unchoked.
        fn unchoked(&self) -> Vec<usize> {
            (0..self.peers.len())
                .filter(|i| self.peers[*i].1.is_some() && self.peers[*i].0.unchoked())
                .collect()
        }

        fn sent(&self, i: usize) -> Vec<bool> {
            self.peers[i].0.sent.lock().clone()
        }
    }

    #[test]
    fn upload_slots_as_text() {
        for (text, slots) in [
            ("auto", UploadSlots::RateBased),
            ("unlimited", UploadSlots::Unlimited),
            ("8", UploadSlots::Fixed(NonZeroU32::new(8).unwrap())),
        ] {
            assert_eq!(text.parse::<UploadSlots>().unwrap(), slots);
            assert_eq!(slots.to_string(), text);
        }
        for text in ["0", "-1", "", "Auto", "4 "] {
            assert!(text.parse::<UploadSlots>().is_err(), "{text:?}");
        }
    }

    #[test]
    fn a_peer_waits_choked_until_it_is_interested() {
        let mut s = Swarm::fixed(2);
        let p = s.connect(0);
        s.rechoke(10);
        assert!(s.sent(p).is_empty(), "a connection starts choked");
        s.interested(p, true, 11);
        s.interested(p, true, 12);
        assert_eq!(s.sent(p), [false], "unchoked at once, and once");
    }

    #[test]
    fn free_slots_are_taken_at_once_and_the_others_wait() {
        let mut s = Swarm::fixed(2);
        s.join(3, 0);
        assert_eq!(s.unchoked(), [0, 1]);
        for round in 1..=2 {
            s.uploaded(0, 100 * KIB);
            s.rechoke(round * 10);
        }
        assert_eq!(s.unchoked(), [0, 1]);
        assert!(s.sent(2).is_empty());
    }

    #[test]
    fn a_slot_nobody_uses_goes_to_a_peer_that_had_none() {
        let mut s = Swarm::fixed(2);
        s.join(3, 0);
        // 0 has the regular slot and takes nothing.
        s.rechoke(10);
        assert_eq!(s.unchoked(), [1, 2]);
    }

    #[test]
    fn a_new_slot_is_not_taken_back_before_its_peer_could_use_it() {
        let mut s = Swarm::fixed(2);
        s.join(4, 0);
        // 2 gets the regular slot a second before the round, 3 never had one.
        s.disconnect(0, 9);
        assert_eq!(s.unchoked(), [1, 2]);
        s.rechoke(10);
        assert_eq!(s.unchoked(), [1, 2]);
        // A round later it is judged like everybody else.
        s.rechoke(20);
        assert_eq!(s.unchoked(), [1, 3]);
    }

    #[test]
    fn a_slot_given_back_goes_to_the_peer_that_waited_the_longest() {
        let mut s = Swarm::fixed(2);
        s.join(4, 0);
        s.interested(0, false, 1);
        assert_eq!(s.sent(0), [false, true], "no interest, no slot");
        assert_eq!(s.unchoked(), [1, 2]);
        s.disconnect(1, 2);
        assert_eq!(s.unchoked(), [2, 3]);
        // The one that had its turn queues behind the one that had none.
        s.interested(0, true, 3);
        let late = s.connect(3);
        s.interested(late, true, 3);
        s.disconnect(2, 4);
        assert_eq!(s.unchoked(), [3, late]);
    }

    #[test]
    fn regular_slots_go_to_the_peers_we_download_from_then_to_the_ones_that_take_the_most() {
        // Three regular slots and an optimistic one.
        let mut s = Swarm::fixed(4);
        s.join(6, 0);
        assert_eq!(s.unchoked(), [0, 1, 2, 3]);
        s.uploaded(0, 100 * KIB);
        s.uploaded(1, 200 * KIB);
        s.uploaded(2, 300 * KIB);
        s.downloaded(4, 50 * KIB);
        s.downloaded(5, 10 * KIB);
        s.rechoke(10);
        // 5 and 4 give us data. 2 takes the most of the others. 3 keeps the optimistic slot.
        assert_eq!(s.unchoked(), [2, 3, 4, 5]);
        assert_eq!(s.sent(0), [false, true]);
        assert_eq!(s.sent(1), [false, true]);
        assert_eq!(s.sent(2), [false], "a kept slot is not announced again");
    }

    #[test]
    fn the_optimistic_slot_moves_every_30s_to_the_peer_that_went_the_longest_without_one() {
        // One regular slot and an optimistic one.
        let mut s = Swarm::fixed(2);
        s.join(4, 0);
        let mut optimistic = Vec::new();
        for round in 1..=12 {
            s.uploaded(0, 100 * KIB);
            s.rechoke(round * 10);
            let unchoked = s.unchoked();
            assert_eq!(unchoked.len(), 2);
            assert_eq!(
                unchoked[0], 0,
                "the peer that takes data keeps the regular slot"
            );
            optimistic.push(unchoked[1]);
        }
        assert_eq!(optimistic, [1, 1, 2, 2, 2, 3, 3, 3, 1, 1, 1, 2]);
    }

    #[test]
    fn an_optimistic_peer_that_earns_a_regular_slot_makes_room_at_once() {
        let mut s = Swarm::fixed(2);
        s.join(3, 0);
        assert_eq!(s.unchoked(), [0, 1]);
        s.uploaded(0, 10 * KIB);
        s.uploaded(1, 500 * KIB);
        s.rechoke(10);
        // 1 took the regular slot from 0, and the optimistic slot it had is 2's without
        // waiting for the rotation.
        assert_eq!(s.unchoked(), [1, 2]);
        assert_eq!(s.sent(1), [false]);
    }

    #[test]
    fn what_a_choked_peer_still_receives_does_not_count() {
        let mut s = Swarm::fixed(2);
        s.join(3, 0);
        for round in 1..=3 {
            s.uploaded(0, 500 * KIB);
            s.rechoke(round * 10);
        }
        // 0 keeps the regular slot, the optimistic one went from 1 to 2.
        assert_eq!(s.unchoked(), [0, 2]);
        // Blocks that were on their way to 1 when it was choked.
        s.uploaded(1, 900 * KIB);
        s.uploaded(0, 100 * KIB);
        s.rechoke(40);
        assert_eq!(s.unchoked(), [0, 2]);
    }

    #[test]
    fn rate_based_slots_follow_the_upload_rate() {
        let mut s = Swarm::new(UploadSlots::RateBased);
        s.join(8, 0);
        assert_eq!(s.unchoked().len(), 4);
        // Everybody with a slot takes 100 KiB/s: one more slot each round.
        for round in 1..=3 {
            for i in s.unchoked() {
                s.uploaded(i, 1000 * KIB);
            }
            s.rechoke(round * 10);
            assert_eq!(s.unchoked().len(), 4 + usize::try_from(round).unwrap());
        }
        // 4 KiB/s each: the third peer is below its bar of 5 KiB/s already.
        for i in s.unchoked() {
            s.uploaded(i, 40 * KIB);
        }
        s.rechoke(40);
        assert_eq!(s.unchoked().len(), 4);
    }

    #[test]
    fn unlimited_slots_unchoke_every_interested_peer() {
        let mut s = Swarm::new(UploadSlots::Unlimited);
        s.join(50, 0);
        s.connect(0);
        assert_eq!(s.unchoked(), (0..50).collect::<Vec<_>>());
        s.rechoke(30);
        assert_eq!(s.unchoked(), (0..50).collect::<Vec<_>>());
    }

    #[test]
    fn an_exempt_peer_is_unchoked_outside_of_the_slots() {
        let mut s = Swarm::fixed(2);
        s.join(3, 0);
        // 0 goes on as it was, without a word. Its slot is 2's.
        s.class(0, SlotClass::Exempt, 1);
        assert_eq!(s.unchoked(), [0, 1, 2]);
        assert_eq!(s.sent(0), [false]);

        // An exempt peer comes and goes with its interest, whatever the slots do.
        let late = s.connect(2);
        s.class(late, SlotClass::Exempt, 2);
        assert!(s.sent(late).is_empty());
        s.interested(late, true, 3);
        for round in 1..=3 {
            s.uploaded(1, 100 * KIB);
            s.uploaded(2, 100 * KIB);
            s.rechoke(round * 10);
        }
        assert_eq!(s.unchoked(), [0, 1, 2, late]);
        s.interested(late, false, 31);
        assert_eq!(s.sent(late), [false, true]);
    }

    #[test]
    fn a_barred_peer_never_gets_a_slot() {
        let mut s = Swarm::fixed(2);
        s.join(3, 0);
        // 0 loses its slot to 2, and does not queue for another one.
        s.class(0, SlotClass::Barred, 1);
        assert_eq!(s.sent(0), [false, true]);
        assert_eq!(s.unchoked(), [1, 2]);
        s.disconnect(1, 2);
        s.rechoke(10);
        s.rechoke(30);
        assert_eq!(s.unchoked(), [2]);
        assert_eq!(s.sent(0), [false, true]);

        // Exempt first, barred later: choked for good as well.
        let p = s.connect(31);
        s.interested(p, true, 31);
        s.class(p, SlotClass::Exempt, 32);
        s.class(p, SlotClass::Barred, 33);
        assert_eq!(s.sent(p), [false, true]);
    }

    // Whatever happens: slots are held by interested contenders only, there are never more
    // holders than slots (unless the slots just shrank under holders too new to be judged),
    // nobody waits while a slot is free, an exempt peer is unchoked exactly while it is
    // interested, a barred one never, and a peer is never told the same thing twice.
    #[test]
    fn slots_stay_consistent_under_random_churn() {
        for (seed, mode) in [
            (1, UploadSlots::Fixed(NonZeroU32::new(1).unwrap())),
            (2, UploadSlots::Fixed(NonZeroU32::new(4).unwrap())),
            (3, UploadSlots::Fixed(NonZeroU32::new(7).unwrap())),
            (4, UploadSlots::RateBased),
        ] {
            let mut rng = 0x9E37_79B9_7F4A_7C15u64.wrapping_mul(seed);
            let mut next = move |below: u64| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                rng % below
            };
            let mut s = Swarm::new(mode);
            let mut interested: Vec<bool> = Vec::new();
            let mut class: Vec<SlotClass> = Vec::new();
            let mut secs = 0;
            let mut slots_before = s.choker.inner.lock().slots;
            for step in 0..4000 {
                let connected: Vec<usize> = (0..s.peers.len())
                    .filter(|i| s.peers[*i].1.is_some())
                    .collect();
                let pick = |n: u64| connected[usize::try_from(n).unwrap() % connected.len()];
                match next(12) {
                    0 | 1 if connected.len() < 30 => {
                        s.connect(secs);
                        interested.push(false);
                        class.push(SlotClass::Contender);
                    }
                    2 if !connected.is_empty() => {
                        let i = pick(next(1000));
                        s.disconnect(i, secs);
                        interested[i] = false;
                    }
                    3..=5 if !connected.is_empty() => {
                        let i = pick(next(1000));
                        interested[i] = next(3) > 0;
                        s.interested(i, interested[i], secs);
                    }
                    6 | 7 if !connected.is_empty() => {
                        let i = pick(next(1000));
                        if s.peers[i].0.unchoked() {
                            s.uploaded(i, next(2000) * KIB);
                        }
                        s.downloaded(i, next(3) * next(500) * KIB);
                    }
                    // The policy only ever gets stricter with a connection.
                    8 if !connected.is_empty() => {
                        let i = pick(next(1000));
                        class[i] = match class[i] {
                            SlotClass::Contender if next(2) == 0 => SlotClass::Exempt,
                            _ => SlotClass::Barred,
                        };
                        s.class(i, class[i], secs);
                    }
                    _ => {
                        let round = secs / 10;
                        secs += 1 + next(4);
                        if secs / 10 > round {
                            slots_before = s.choker.inner.lock().slots;
                            s.rechoke(secs);
                        }
                    }
                }

                let unchoked = s.unchoked();
                let slots = s.choker.inner.lock().slots;
                let ctx = format!("seed {seed}, step {step}");
                let holders = unchoked
                    .iter()
                    .filter(|i| class[**i] == SlotClass::Contender)
                    .count();
                assert!(
                    holders <= slots.max(slots_before),
                    "{ctx}: more holders than slots"
                );
                let mut waiting = 0;
                for i in 0..s.peers.len() {
                    let is_unchoked = unchoked.contains(&i);
                    match class[i] {
                        SlotClass::Contender => {
                            assert!(
                                interested[i] || !is_unchoked,
                                "{ctx}: {i} has a slot it does not want"
                            );
                            waiting += usize::from(interested[i] && !is_unchoked);
                        }
                        SlotClass::Exempt => {
                            assert_eq!(is_unchoked, interested[i], "{ctx}: exempt {i}")
                        }
                        SlotClass::Barred => assert!(!is_unchoked, "{ctx}: barred {i}"),
                    }
                }
                assert!(
                    waiting == 0 || holders >= slots,
                    "{ctx}: {waiting} peers wait while a slot is free"
                );
                for (peer, _) in &s.peers {
                    let sent = peer.sent.lock();
                    assert!(
                        sent.first() != Some(&true) && sent.windows(2).all(|w| w[0] != w[1]),
                        "{ctx}: repeated choke or unchoke {sent:?}"
                    );
                }
            }
        }
    }
}
