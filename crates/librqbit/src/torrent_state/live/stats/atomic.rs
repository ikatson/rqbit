use std::sync::atomic::AtomicU64;

#[derive(Default, Debug)]
pub struct AtomicStats {
    pub have_bytes: AtomicU64,
    pub downloaded_and_checked_bytes: AtomicU64,
    pub downloaded_and_checked_pieces: AtomicU64,
    pub uploaded_bytes: AtomicU64,
    pub fetched_bytes: AtomicU64,
    pub total_piece_download_ms: AtomicU64,
}
