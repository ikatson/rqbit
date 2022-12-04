use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

#[derive(Debug)]
pub(crate) struct PeerConnectionStats {
    pub created_at: Instant,
    pub connected_at: Option<Instant>,
    pub dropped_at: Option<Instant>,
    pub received_bytes: AtomicU64,
}

impl Clone for PeerConnectionStats {
    fn clone(&self) -> Self {
        Self {
            created_at: self.created_at,
            connected_at: self.connected_at,
            dropped_at: self.dropped_at,
            received_bytes: AtomicU64::new(self.received_bytes.load(Ordering::Relaxed)),
        }
    }
}

impl PeerConnectionStats {
    pub fn new() -> Self {
        Self {
            created_at: Instant::now(),
            connected_at: None,
            dropped_at: None,
            received_bytes: Default::default(),
        }
    }

    pub fn mark_connected(&mut self) {
        self.connected_at = Some(Instant::now())
    }

    pub fn add_received_bytes(&self, len: u64, ordering: Ordering) {
        self.received_bytes.fetch_add(len, ordering);
    }

    pub fn mark_dropped(&mut self) {
        self.dropped_at = Some(Instant::now())
    }

    pub fn time_alive(&self) -> Duration {
        let end = self.dropped_at.unwrap_or_else(Instant::now);
        end.duration_since(self.created_at)
    }

    pub fn connection_time(&self) -> Option<Duration> {
        let conn = self.connected_at?;
        Some(conn.duration_since(self.created_at))
    }

    pub fn avg_download_speed_bps(&self) -> f64 {
        let conn_time = match self.connection_time() {
            Some(conn_time) => conn_time,
            None => return 0f64,
        };

        (self.received_bytes.load(Ordering::Relaxed) as f64) / conn_time.as_secs_f64()
    }

    pub fn quality(&self) -> f64 {
        let connection_time_quality = match self.connection_time() {
            Some(conn_time) => f64::min(2f64, 1f64 / conn_time.as_secs_f64()),
            None => 0f64,
        };

        let time_alive_quality = {
            let time_alive = self.time_alive().as_secs_f64();
            f64::min(1., 2. - 1. / (time_alive * 0.01 + 0.5))
        };

        // ~6mbps = 1,
        // \ln\ \left(x\ +\ 1\right)\ \cdot\ 0.5
        let speed_quality = {
            let avg_speed_mbps = self.avg_download_speed_bps() / (1024f64 * 1024f64);
            (avg_speed_mbps + 1f64).ln()
        };

        connection_time_quality * speed_quality * time_alive_quality
    }
}

impl Default for PeerConnectionStats {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::atomic::Ordering, time::Duration};

    use super::PeerConnectionStats;

    #[test]
    fn test_quality_1() {
        let mut s = PeerConnectionStats::new();
        s.connected_at = Some(s.created_at + Duration::from_secs(1));
        s.dropped_at = Some(s.connected_at.unwrap() + Duration::from_secs(60));
        s.add_received_bytes(240 * 1024 * 1024, Ordering::Relaxed);
        println!("{}", s.quality());
    }
}
