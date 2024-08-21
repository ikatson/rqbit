use std::sync::atomic::AtomicU64;

use crate::torrent_state::live::peers::stats::atomic::AggregatePeerStatsAtomic;

#[derive(Default, Debug)]
pub struct AtomicSessionStats {
    pub fetched_bytes: AtomicU64,
    pub uploaded_bytes: AtomicU64,
    pub(crate) peers: AggregatePeerStatsAtomic,
}
