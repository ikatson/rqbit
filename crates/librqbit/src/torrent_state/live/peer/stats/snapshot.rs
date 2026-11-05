use std::{collections::HashMap, sync::atomic::Ordering};

use serde::{Deserialize, Serialize};

use crate::{
    stream_connect::ConnectionKind,
    torrent_state::live::peer::{Peer, PeerState},
};

#[derive(Serialize, Deserialize)]
pub struct PeerCounters {
    pub incoming_connections: u32,
    pub fetched_bytes: u64,
    pub uploaded_bytes: u64,
    pub total_time_connecting_ms: u64,
    pub connection_attempts: u32,
    pub connections: u32,
    pub errors: u32,
    pub fetched_chunks: u32,
    pub downloaded_and_checked_pieces: u32,
    pub total_piece_download_ms: u64,
    pub times_stolen_from_me: u32,
    pub times_i_stole: u32,
}

#[derive(Serialize)]
pub struct PeerStats {
    pub counters: PeerCounters,
    pub state: &'static str,
    pub conn_kind: Option<ConnectionKind>,
    pub client_name: Option<String>,
    /// How many pieces this peer has.
    ///
    /// `None` unless [`PeerStatsFilter::include_bitfield`] was set and the peer
    /// is live. Provided alongside the bitfield to save every caller the same
    /// `count_ones()`.
    ///
    /// Distinct from `counters.downloaded_and_checked_pieces`, which is how
    /// many pieces this peer has sent *us*.
    pub have_pieces: Option<u32>,
    /// This peer's bitfield.
    ///
    /// `None` unless [`PeerStatsFilter::include_bitfield`] was set and the peer
    /// is live. The trailing padding is cleared on ingest, so the bits are
    /// exactly the pieces the peer holds.
    ///
    /// Raw bytes rather than the internal `BF`, because this struct is
    /// `Serialize` and `bitvec` is built without its `serde` feature — and
    /// naming `BitBox` here would put `bitvec` in the public API, where a
    /// dependent would need a matching version to read the field.
    ///
    /// A count is not a substitute: computing piece availability across a
    /// swarm — the rarest-piece copy count — needs to know *which* pieces each
    /// peer holds. Two peers with 500 pieces each may overlap entirely or not
    /// at all, and only one of those torrents can finish.
    pub have_bitfield: Option<Vec<u8>>,
}

impl From<&super::atomic::PeerCountersAtomic> for PeerCounters {
    fn from(counters: &super::atomic::PeerCountersAtomic) -> Self {
        Self {
            incoming_connections: counters.incoming_connections.load(Ordering::Relaxed),
            fetched_bytes: counters.fetched_bytes.load(Ordering::Relaxed),
            uploaded_bytes: counters.uploaded_bytes.load(Ordering::Relaxed),
            total_time_connecting_ms: counters.total_time_connecting_ms.load(Ordering::Relaxed),
            connection_attempts: counters
                .outgoing_connection_attempts
                .load(Ordering::Relaxed),
            connections: counters.outgoing_connections.load(Ordering::Relaxed),
            errors: counters.errors.load(Ordering::Relaxed),
            fetched_chunks: counters.fetched_chunks.load(Ordering::Relaxed),
            downloaded_and_checked_pieces: counters
                .downloaded_and_checked_pieces
                .load(Ordering::Relaxed),
            total_piece_download_ms: counters.total_piece_download_ms.load(Ordering::Relaxed),
            times_i_stole: counters.times_i_stole.load(Ordering::Relaxed),
            times_stolen_from_me: counters.times_stolen_from_me.load(Ordering::Relaxed),
        }
    }
}

impl PeerStats {
    /// Builds a snapshot for one peer.
    pub(crate) fn from_peer(peer: &Peer, include_bitfield: bool) -> Self {
        let state = peer.get_state();
        Self {
            counters: peer.stats.counters.as_ref().into(),
            state: state.name(),
            conn_kind: match state {
                PeerState::Live(l) => Some(l.connection_kind),
                _ => None,
            },
            client_name: match state {
                PeerState::Live(l) => l.client_name.clone(),
                _ => None,
            },
            have_pieces: match state {
                // The bitfield is sized from total_pieces, so the count fits a
                // u32 by construction.
                PeerState::Live(l) if include_bitfield => {
                    Some(u32::try_from(l.bitfield.count_ones()).unwrap_or(u32::MAX))
                }
                _ => None,
            },
            have_bitfield: match state {
                PeerState::Live(l) if include_bitfield => Some(l.bitfield.as_raw_slice().to_vec()),
                _ => None,
            },
        }
    }
}

#[derive(Serialize)]
pub struct PeerStatsSnapshot {
    pub peers: HashMap<String, PeerStats>,
}

#[derive(Clone, Copy, Default, Deserialize)]
pub enum PeerStatsFilterState {
    #[serde(rename = "all")]
    All,
    #[default]
    #[serde(rename = "live")]
    Live,
}

impl PeerStatsFilterState {
    pub(crate) fn matches(&self, s: &PeerState) -> bool {
        matches!((self, s), (Self::All, _) | (Self::Live, PeerState::Live(_)))
    }
}

#[derive(Default, Deserialize)]
pub struct PeerStatsFilter {
    #[serde(default)]
    pub state: PeerStatsFilterState,
    /// Populate `have_pieces` and `have_bitfield`.
    ///
    /// Off by default, and nothing is computed unless it is set: the bitfield
    /// costs a byte per eight pieces per peer to copy, and counting its bits
    /// is a scan the existing callers have no use for.
    #[serde(default)]
    pub include_bitfield: bool,
}

#[cfg(test)]
mod tests {
    use super::PeerStatsFilter;

    /// The default filter computes nothing, so existing callers pay nothing.
    #[test]
    fn bitfields_are_off_by_default() {
        assert!(!PeerStatsFilter::default().include_bitfield);
    }
}
