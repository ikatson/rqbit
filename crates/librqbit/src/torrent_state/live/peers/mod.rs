use std::{collections::HashSet, net::SocketAddr, sync::Arc, sync::atomic::Ordering};

use tracing::debug;

use dashmap::DashMap;
use librqbit_core::lengths::ValidPieceIndex;
use parking_lot::RwLock;
use peer_binary_protocol::{Message, Request};

use crate::{
    Error,
    peer_connection::WriterRequest,
    torrent_state::utils::{TimedExistence, atomic_inc},
    type_aliases::{BF, PeerHandle},
};

use self::stats::{AggregatePeerStats, AggregatePeerStatsAtomic};

use super::peer::{LivePeerState, Peer, PeerRx, PeerState, PeerTx};

pub mod stats;

pub(crate) struct PeerStates {
    pub session_stats: Arc<AggregatePeerStatsAtomic>,

    // This keeps track of live addresses we connected to, for PEX.
    pub live_outgoing_peers: RwLock<HashSet<PeerHandle>>,
    pub stats: AggregatePeerStatsAtomic,
    pub states: DashMap<PeerHandle, Peer>,
}

impl Drop for PeerStates {
    fn drop(&mut self) {
        for (_, p) in std::mem::take(&mut self.states).into_iter() {
            p.destroy(self);
        }
    }
}

impl PeerStates {
    pub fn stats(&self) -> AggregatePeerStats {
        self.stats.snapshot()
    }

    pub fn add_if_not_seen(&self, addr: SocketAddr) -> Option<PeerHandle> {
        use dashmap::mapref::entry::Entry;
        match self.states.entry(addr) {
            Entry::Occupied(_) => None,
            Entry::Vacant(vac) => {
                vac.insert(Peer::new_with_outgoing_address(addr));
                atomic_inc(&self.stats.queued);
                atomic_inc(&self.session_stats.queued);

                atomic_inc(&self.stats.seen);
                atomic_inc(&self.session_stats.seen);
                Some(addr)
            }
        }
    }
    pub fn with_peer<R>(&self, addr: PeerHandle, f: impl FnOnce(&Peer) -> R) -> Option<R> {
        self.states.get(&addr).map(|e| f(e.value()))
    }

    pub fn with_peer_mut<R>(
        &self,
        addr: PeerHandle,
        reason: &'static str,
        f: impl FnOnce(&mut Peer) -> R,
    ) -> Option<R> {
        use crate::torrent_state::utils::timeit;
        timeit(reason, || self.states.get_mut(&addr))
            .map(|e| f(TimedExistence::new(e, reason).value_mut()))
    }

    pub fn with_live<R>(&self, addr: PeerHandle, f: impl FnOnce(&LivePeerState) -> R) -> Option<R> {
        self.with_peer(addr, |peer| peer.get_live().map(f))
            .flatten()
    }

    pub fn with_live_mut<R>(
        &self,
        addr: PeerHandle,
        reason: &'static str,
        f: impl FnOnce(&mut LivePeerState) -> R,
    ) -> Option<R> {
        self.with_peer_mut(addr, reason, |peer| peer.get_live_mut().map(f))
            .flatten()
    }

    pub fn drop_peer(&self, handle: PeerHandle) -> Option<Peer> {
        let p = self.states.remove(&handle).map(|r| r.1)?;
        let s = p.get_state();
        self.stats.dec(s);
        self.session_stats.dec(s);

        Some(p)
    }

    pub fn is_peer_not_interested_and_has_full_torrent(
        &self,
        handle: PeerHandle,
        total_pieces: usize,
    ) -> bool {
        self.with_live(handle, |live| {
            !live.peer_interested && live.has_full_torrent(total_pieces)
        })
        .unwrap_or(false)
    }

    pub fn mark_peer_interested(&self, handle: PeerHandle, is_interested: bool) -> Option<bool> {
        self.with_live_mut(handle, "mark_peer_interested", |live| {
            let prev = live.peer_interested;
            live.peer_interested = is_interested;
            prev
        })
    }

    pub fn update_bitfield(&self, handle: PeerHandle, bitfield: BF) -> Option<()> {
        self.with_live_mut(handle, "update_bitfield", |live| {
            live.bitfield = bitfield;
        })
    }

    pub fn mark_peer_connecting(&self, h: PeerHandle) -> crate::Result<(PeerRx, PeerTx)> {
        let rx = self
            .with_peer_mut(h, "mark_peer_connecting", |peer| {
                peer.idle_to_connecting(self)
                    .ok_or(Error::BugInvalidPeerState)
            })
            .ok_or(Error::PeerNotFound)??;
        Ok(rx)
    }

    pub fn reset_peer_backoff(&self, handle: PeerHandle) {
        self.with_peer_mut(handle, "reset_peer_backoff", |p| {
            p.stats.reset_backoff();
        });
    }

    pub fn mark_peer_not_needed(&self, handle: PeerHandle) -> Option<PeerState> {
        let prev = self.with_peer_mut(handle, "mark_peer_not_needed", |peer| {
            peer.set_not_needed(self)
        })?;
        Some(prev)
    }

    pub(crate) fn on_steal(
        &self,
        from_peer: SocketAddr,
        to_peer: SocketAddr,
        stolen_idx: ValidPieceIndex,
    ) {
        self.with_peer(to_peer, |p| {
            atomic_inc(&p.stats.counters.times_i_stole);
        });
        self.with_peer(from_peer, |p| {
            atomic_inc(&p.stats.counters.times_stolen_from_me);
        });
        self.stats.inc_steals();
        self.session_stats.inc_steals();

        self.with_live_mut(from_peer, "send_cancellations", |live| {
            let tx = &live.tx;
            live.inflight_requests.retain(|req| {
                if req.piece_index == stolen_idx {
                    let _ = tx.send(WriterRequest::Message(Message::Cancel(Request {
                        index: stolen_idx.get(),
                        begin: req.offset,
                        length: req.size,
                    })));
                    false
                } else {
                    true
                }
            });
        });
    }

    /// Remove excess peers when the count exceeds `max_peers`.
    ///
    /// Removal priority:
    /// 1. NotNeeded peers — outright delete (kept only for statistics, no value)
    /// 2. Dead / Queued peers — scored by worst connection history
    ///
    /// Live peers are never pruned — they are already bounded by the
    /// per-torrent connection limit and represent active data transfer.
    ///
    /// Score heuristic for Dead/Queued:
    ///   score = (errors × 100 + connection_attempts × 10) / max(fetched_kb, 1)
    /// Higher score = worse peer = pruned first.
    pub fn prune_peers(&self, max_peers: usize) -> usize {
        let total = self.states.len();
        if total <= max_peers {
            return 0;
        }
        let to_remove = total - max_peers;
        let mut removed = 0;

        // Phase 1: Delete NotNeeded peers outright (statistics only, no value).
        // Re-check state before removal to avoid races — a peer may have
        // transitioned out of NotNeeded between the iteration and the removal.
        let not_needed: Vec<PeerHandle> = self
            .states
            .iter()
            .filter(|entry| matches!(entry.value().get_state(), PeerState::NotNeeded))
            .map(|entry| *entry.key())
            .collect();

        for handle in not_needed {
            if removed >= to_remove {
                break;
            }
            // Re-check: only drop if still NotNeeded
            let still_not_needed = self
                .states
                .get(&handle)
                .map(|e| matches!(e.value().get_state(), PeerState::NotNeeded))
                .unwrap_or(false);
            if still_not_needed {
                if self.drop_peer(handle).is_some() {
                    removed += 1;
                }
            }
        }

        if removed >= to_remove {
            if removed > 0 {
                debug!(removed, "pruned peers");
            }
            return removed;
        }

        // Phase 2: Score Dead and Queued peers by worst connection history.
        // Peers that never connected, never uploaded, or have high error rates
        // are pruned first. Live peers are never touched.
        let mut scored: Vec<(PeerHandle, u64)> = self
            .states
            .iter()
            .filter(|entry| {
                matches!(
                    entry.value().get_state(),
                    PeerState::Dead | PeerState::Queued
                )
            })
            .map(|entry| {
                let counters = &entry.value().stats.counters;
                let errors = counters.errors.load(Ordering::Relaxed) as u64;
                let attempts = counters
                    .outgoing_connection_attempts
                    .load(Ordering::Relaxed) as u64;
                let fetched_kb = counters.fetched_bytes.load(Ordering::Relaxed) / 1024;
                // Higher score = worse peer (more errors, fewer bytes transferred)
                let score = (errors * 100 + attempts * 10) / fetched_kb.max(1);
                (*entry.key(), score)
            })
            .collect();

        // Sort by score descending (worst peers first).
        // Unstable sort is sufficient — peer handle order doesn't matter for ties.
        scored.sort_unstable_by_key(|&(_, score)| std::cmp::Reverse(score));

        for (handle, _score) in scored {
            if removed >= to_remove {
                break;
            }
            // Re-check: only drop if still Dead or Queued
            let still_prunable = self
                .states
                .get(&handle)
                .map(|e| matches!(e.value().get_state(), PeerState::Dead | PeerState::Queued))
                .unwrap_or(false);
            if still_prunable {
                if self.drop_peer(handle).is_some() {
                    removed += 1;
                }
            }
        }

        if removed > 0 {
            debug!(removed, "pruned peers");
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    /// Helper to compute the pruning score for a peer, matching the production formula.
    fn compute_score(errors: u64, attempts: u64, fetched_bytes: u64) -> u64 {
        let fetched_kb = fetched_bytes / 1024;
        (errors * 100 + attempts * 10) / fetched_kb.max(1)
    }

    #[test]
    fn test_score_zero_bytes_peer_is_worst() {
        // A peer with errors but no data fetched should score highest (worst).
        let score_no_data = compute_score(5, 10, 0);
        let score_with_data = compute_score(5, 10, 1024 * 1024); // 1 MB
        assert!(
            score_no_data > score_with_data,
            "peer with no data should score worse: {} vs {}",
            score_no_data,
            score_with_data
        );
    }

    #[test]
    fn test_score_more_errors_is_worse() {
        let score_few = compute_score(1, 5, 50 * 1024);
        let score_many = compute_score(10, 5, 50 * 1024);
        assert!(
            score_many > score_few,
            "more errors should score worse: {} vs {}",
            score_many,
            score_few
        );
    }

    #[test]
    fn test_score_more_data_is_better() {
        let score_little = compute_score(3, 5, 10 * 1024);
        let score_lots = compute_score(3, 5, 10 * 1024 * 1024);
        assert!(
            score_little > score_lots,
            "less data should score worse: {} vs {}",
            score_little,
            score_lots
        );
    }

    #[test]
    fn test_sort_order_worst_first() {
        // Simulate scored peers and verify sort order.
        let mut scored = vec![
            (addr(1), compute_score(1, 1, 1024 * 1024)), // good peer
            (addr(2), compute_score(10, 20, 0)),         // worst peer
            (addr(3), compute_score(5, 5, 100 * 1024)),  // mediocre peer
        ];
        scored.sort_unstable_by_key(|&(_, score)| std::cmp::Reverse(score));

        // Worst peer (addr(2)) should come first
        assert_eq!(scored[0].0, addr(2));
        // Good peer (addr(1)) should come last
        assert_eq!(scored[scored.len() - 1].0, addr(1));
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }
}
