use std::{
    collections::{HashMap, VecDeque},
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

use parking_lot::Mutex;

use crate::{peer_stats::PeerConnectionStats, type_aliases::PeerHandle};

const MAX_LAST_STATES: usize = 3;

#[derive(Default)]
struct PeerRequeueInfo {
    successive_quick_failures: usize,
    successive_connection_failures: usize,
    last_qualities: VecDeque<f64>,
}

struct PeerRequeueInfoAggregated {
    successive_quick_failures: usize,
    successive_connection_failures: usize,
    avg_connection_quality: f64,
}

impl PeerRequeueInfo {
    fn avg_quality_unchecked(&self) -> f64 {
        self.last_qualities.iter().fold(0f64, |acc, s| acc + *s)
            / (self.last_qualities.len() as f64)
    }

    fn add_stats(&mut self, stats: &PeerConnectionStats) -> PeerRequeueInfoAggregated {
        let quality = stats.quality();
        if self.last_qualities.len() == MAX_LAST_STATES {
            self.last_qualities.pop_front();
        }
        self.last_qualities.push_back(quality);

        if stats.received_bytes.load(Ordering::Relaxed) == 0 {
            self.successive_quick_failures += 1;
        } else {
            self.successive_quick_failures = 0;
        }

        if stats.connected_at.is_none() {
            self.successive_connection_failures += 1;
        } else {
            self.successive_connection_failures = 0;
        }

        let avg_quality = self.avg_quality_unchecked();
        PeerRequeueInfoAggregated {
            successive_quick_failures: self.successive_quick_failures,
            successive_connection_failures: self.successive_connection_failures,
            avg_connection_quality: avg_quality,
        }
    }
}

#[derive(Default)]
struct RequeueManagerInner {
    seen_peers: HashMap<PeerHandle, PeerRequeueInfo>,
}

pub(crate) struct RequeueManager {
    inner: Arc<Mutex<RequeueManagerInner>>,
}

fn jitter(d: f64) -> f64 {
    let r: f64 = rand::random();
    let factor = 0.5 + r * 0.5;
    d * factor
}

impl RequeueManager {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RequeueManagerInner::default())),
        }
    }

    // Returns duration to requeue the peer in.
    pub fn on_peer_dropped(
        &self,
        handle: PeerHandle,
        stats: &PeerConnectionStats,
    ) -> Option<Duration> {
        let agg_peer_stats = {
            let mut g = self.inner.lock();
            let ps = g.seen_peers.entry(handle).or_default();
            ps.add_stats(stats)
        };
        if agg_peer_stats.successive_connection_failures >= 3 {
            // Give up on peer after 3 connection attempts.
            return None;
        }
        // Calculate reconnect seconds based on peer quality.
        let reconnect_seconds = f64::min(10f64 / agg_peer_stats.avg_connection_quality, 60f64);
        // Multiply with exponential backoff based on successive quick failures.
        let reconnect_seconds = reconnect_seconds
            * (u64::pow(
                2,
                std::cmp::max(agg_peer_stats.successive_quick_failures as u32, 3),
            ) as f64);
        // Add jitter.
        let reconnect_seconds = jitter(reconnect_seconds);
        Some(Duration::from_secs_f64(reconnect_seconds))
    }
}
