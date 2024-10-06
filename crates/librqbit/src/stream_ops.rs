
const MAX_STORED_CONTENT_SIZE: usize = 100 * 1024 * 1024;   // 100 MB

/// Data structure representing what can be tweaked when streaming data.
pub struct StreamOptions {
    /// Once served to client, data is removed from memory (and thus needs to be re-fetched if needed again)
    pub erase_content_after_served: bool,
    /// Maximum size allowed to be stored in the `TorrentStorage` before throttling.
    /// Allows a finer control over what memory quantity can be used.
    pub max_stored_content_size: usize
}

impl Default for StreamOptions {
    fn default() -> Self {
        Self {
            erase_content_after_served: false,
            max_stored_content_size: MAX_STORED_CONTENT_SIZE
        }
    }
}
