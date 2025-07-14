use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
struct ProgressSnapshot {
    progress_bytes: u64,
    instant: Instant,
}

struct Shared {
    bytes_per_second: AtomicU64,
    time_remaining_millis: AtomicU64,
}

pub struct Updater {
    snapshots: VecDeque<ProgressSnapshot>,
    shared: Arc<Shared>,
}

/// Estimates download/upload speed in a sliding time window.
pub struct SpeedEstimator {
    shared: Arc<Shared>,
}

impl SpeedEstimator {
    pub fn new(capacity: usize) -> (SpeedEstimator, Updater) {
        assert!(capacity > 1);
        let shared = Arc::new(Shared {
            bytes_per_second: Default::default(),
            time_remaining_millis: Default::default(),
        });
        (
            SpeedEstimator {
                shared: shared.clone(),
            },
            Updater {
                snapshots: VecDeque::with_capacity(capacity),
                shared,
            },
        )
    }

    pub fn time_remaining(&self) -> Option<Duration> {
        let tr = self.shared.time_remaining_millis.load(Ordering::Relaxed);
        if tr == 0 {
            return None;
        }
        Some(Duration::from_millis(tr))
    }

    pub fn bps(&self) -> u64 {
        self.shared.bytes_per_second.load(Ordering::Relaxed)
    }

    pub fn mbps(&self) -> f64 {
        self.bps() as f64 / 1024f64 / 1024f64
    }
}

impl Updater {
    pub fn add_snapshot(
        &mut self,
        progress_bytes: u64,
        remaining_bytes: Option<u64>,
        instant: Instant,
    ) {
        let first = {
            let current = ProgressSnapshot {
                progress_bytes,
                instant,
            };

            if self.snapshots.is_empty() {
                self.snapshots.push_back(current);
                return;
            } else if self.snapshots.len() < self.snapshots.capacity() {
                self.snapshots.push_back(current);
                self.snapshots.front().copied().unwrap()
            } else {
                let first = self.snapshots.pop_front().unwrap();
                self.snapshots.push_back(current);
                first
            }
        };

        let downloaded_bytes_diff = progress_bytes - first.progress_bytes;
        let elapsed = instant - first.instant;
        let bps = downloaded_bytes_diff as f64 / elapsed.as_secs_f64();

        let time_remaining_millis_rounded: u64 = if downloaded_bytes_diff > 0 {
            let time_remaining_secs = remaining_bytes.unwrap_or_default() as f64 / bps;
            (time_remaining_secs * 1000f64) as u64
        } else {
            0
        };
        self.shared
            .time_remaining_millis
            .store(time_remaining_millis_rounded, Ordering::Relaxed);
        self.shared
            .bytes_per_second
            .store(bps as u64, Ordering::Relaxed);
    }
}
