use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

#[derive(Default, Debug)]
pub struct AtomicStats {
    pub have_bytes: AtomicU64,
    pub downloaded_and_checked_bytes: AtomicU64,
    pub downloaded_and_checked_pieces: AtomicU64,
    pub uploaded_bytes: AtomicU64,
    pub fetched_bytes: AtomicU64,
    pub total_piece_download_ms: AtomicU64,
}

impl AtomicStats {
    pub fn average_piece_download_time(&self) -> Option<Duration> {
        let d = self.downloaded_and_checked_pieces.load(Ordering::Acquire);
        let t = self.total_piece_download_ms.load(Ordering::Acquire);
        if d == 0 {
            return None;
        }
        Some(Duration::from_secs_f64(t as f64 / d as f64 / 1000f64))
    }
}
