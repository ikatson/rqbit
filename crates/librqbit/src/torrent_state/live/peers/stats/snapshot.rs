use std::sync::atomic::Ordering;

use serde::Serialize;

use super::atomic::AggregatePeerStatsAtomic;

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct AggregatePeerStats {
    pub queued: usize,
    pub connecting: usize,
    pub live: usize,
    pub seen: usize,
    pub dead: usize,
    pub not_needed: usize,
}

impl<'a> From<&'a AggregatePeerStatsAtomic> for AggregatePeerStats {
    fn from(s: &'a AggregatePeerStatsAtomic) -> Self {
        let ordering = Ordering::Relaxed;
        Self {
            queued: s.queued.load(ordering) as usize,
            connecting: s.connecting.load(ordering) as usize,
            live: s.live.load(ordering) as usize,
            seen: s.seen.load(ordering) as usize,
            dead: s.dead.load(ordering) as usize,
            not_needed: s.not_needed.load(ordering) as usize,
        }
    }
}
