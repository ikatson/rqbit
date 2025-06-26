use std::sync::atomic::Ordering;

use serde::Serialize;

use super::atomic::AggregatePeerStatsAtomic;

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct AggregatePeerStats {
    pub queued: u32,
    pub connecting: u32,
    pub live: u32,
    pub live_tcp: u32,
    pub live_utp: u32,
    pub live_socks: u32,
    pub seen: u32,
    pub dead: u32,
    pub not_needed: u32,
    pub steals: u32,
}

impl<'a> From<&'a AggregatePeerStatsAtomic> for AggregatePeerStats {
    fn from(s: &'a AggregatePeerStatsAtomic) -> Self {
        let ordering = Ordering::Relaxed;
        Self {
            queued: s.queued.load(ordering),
            connecting: s.connecting.load(ordering),
            live: s.live.load(ordering),
            live_tcp: s.live_tcp.load(ordering),
            live_utp: s.live_utp.load(ordering),
            live_socks: s.live_socks.load(ordering),
            seen: s.seen.load(ordering),
            dead: s.dead.load(ordering),
            not_needed: s.not_needed.load(ordering),
            steals: s.steals.load(ordering),
        }
    }
}
