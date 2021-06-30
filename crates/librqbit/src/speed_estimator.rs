use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use parking_lot::Mutex;
use tokio::time::sleep;

use crate::torrent_state::{StatsSnapshot, TorrentState};

pub struct SpeedEstimator {
    state: Arc<TorrentState>,
    latest_per_second_snapshots: Mutex<VecDeque<StatsSnapshot>>,
    download_bytes_per_second: AtomicU64,
    time_remaining_millis: AtomicU64,
}

impl SpeedEstimator {
    pub fn new(state: Arc<TorrentState>, window_seconds: usize) -> Arc<Self> {
        assert!(window_seconds > 1);
        let estimator = Arc::new(Self {
            state,
            latest_per_second_snapshots: Mutex::new(VecDeque::with_capacity(window_seconds)),
            download_bytes_per_second: Default::default(),
            time_remaining_millis: Default::default(),
        });
        estimator
    }

    pub fn time_remaining(&self) -> Option<Duration> {
        let tr = self.time_remaining_millis.load(Ordering::Relaxed);
        if tr == 0 {
            return None;
        }
        Some(Duration::from_millis(tr))
    }

    pub fn download_bps(&self) -> u64 {
        self.download_bytes_per_second.load(Ordering::Relaxed)
    }

    pub fn download_mbps(&self) -> f64 {
        self.download_bps() as f64 / 1024f64 / 1024f64
    }

    pub async fn run_forever(self: Arc<Self>) -> anyhow::Result<()> {
        loop {
            let current = self.state.stats_snapshot();
            {
                let mut g = self.latest_per_second_snapshots.lock();
                if g.len() < g.capacity() {
                    g.push_back(current);
                    continue;
                }
                let first = g.pop_front().unwrap();

                let downloaded_bytes =
                    current.downloaded_and_checked_bytes - first.downloaded_and_checked_bytes;
                let elapsed = first.time.elapsed();
                let bps = downloaded_bytes as f64 / elapsed.as_secs_f64();

                let time_remaining_millis_rounded: u64 = if downloaded_bytes > 0 {
                    let time_remaining_secs = current.remaining_bytes as f64 / bps;
                    (time_remaining_secs * 1000f64) as u64
                } else {
                    0
                };
                self.time_remaining_millis
                    .store(time_remaining_millis_rounded, Ordering::Relaxed);
                self.download_bytes_per_second
                    .store(bps as u64, Ordering::Relaxed);

                g.push_back(current);
            }

            sleep(Duration::from_secs(1)).await;
        }
    }
}
