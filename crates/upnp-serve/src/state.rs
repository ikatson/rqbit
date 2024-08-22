use std::sync::Arc;

use axum::body::Bytes;

pub struct UnpnServerStateInner {
    pub usn: String,
    pub friendly_name: String,
    pub server_header_string: String,
    pub port: u16,
    pub rendered_root_description: Bytes,
    pub provider: Box<dyn ContentDirectoryBrowseProvider>,
}

pub type UnpnServerState = Arc<UnpnServerStateInner>;

#[derive(Debug, Clone)]
pub struct ContentDirectoryBrowseItem {
    pub title: String,
    pub mime_type: Option<String>,
    pub url: String,
}

pub trait ContentDirectoryBrowseProvider: Send + Sync {
    fn browse(&self) -> Vec<ContentDirectoryBrowseItem>;
}

impl ContentDirectoryBrowseProvider for Vec<ContentDirectoryBrowseItem> {
    fn browse(&self) -> Vec<ContentDirectoryBrowseItem> {
        self.clone()
    }
}
