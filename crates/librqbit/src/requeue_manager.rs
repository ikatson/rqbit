use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use parking_lot::Mutex;

use crate::{peer_stats::PeerConnectionStats, type_aliases::PeerHandle};

const MAX_LAST_STATES: usize = 3;

type PeerDroppedMsg = (PeerHandle, PeerConnectionStats);

#[derive(Default)]
struct PeerRequeueInfo {
    dropped_times: u64,
    last_qualities: VecDeque<f64>,
}

struct PeerRequeueInfoAggregated {
    dropped_times: u64,
    successive_quick_failures: u64,
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
        self.dropped_times += 1;

        let avg_quality = self.avg_quality_unchecked();
        PeerRequeueInfoAggregated {
            dropped_times: self.dropped_times,
            // TODO
            successive_quick_failures: self.dropped_times,
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

// For each peer:
//
// If peer dropped too quickly (like immediately after connection), increase the time for requeue
// If peer keeps dropping quickly, stop requeing it (or increase the timeout drastically)
// The better the peer, the faster it should requeue.
// Each subsequent requeue should increase the timeout.
// If couldn't connect at all (ever), increase the timeout.
//
// However given all this, if the network changes or whatever, and everyone drops, don't punish everyone
// too much.
//
// Jitter on requeue.

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
    pub async fn start(self: Arc<Self>) -> anyhow::Result<()> {
        Ok(())
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
        let base_reconnect_seconds = f64::min(10f64 / agg_peer_stats.avg_connection_quality, 60f64);
        let reconnect_seconds = base_reconnect_seconds
            * (u64::pow(2, agg_peer_stats.successive_quick_failures as u32) as f64);
        let reconnect_seconds = jitter(reconnect_seconds);
        Some(Duration::from_secs_f64(reconnect_seconds))
    }
}
