use std::sync::Arc;

use axum::body::Bytes;

pub struct UnpnServerStateInner {
    pub rendered_root_description: Bytes,
    pub provider: Box<dyn ContentDirectoryBrowseProvider>,
}

pub type UnpnServerState = Arc<UnpnServerStateInner>;

#[derive(Debug, Clone)]
pub struct Container {
    pub id: usize,
    pub parent_id: Option<usize>,
    pub children_count: usize,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct Item {
    pub id: usize,
    pub parent_id: Option<usize>,
    pub title: String,
    pub mime_type: Option<mime_guess::Mime>,
    pub url: String,
}

#[derive(Debug, Clone)]
pub enum ContentDirectoryBrowseItem {
    Container(Container),
    Item(Item),
}

pub trait ContentDirectoryBrowseProvider: Send + Sync {
    fn browse_direct_children(&self, parent_id: usize) -> Vec<ContentDirectoryBrowseItem>;
}

impl ContentDirectoryBrowseProvider for Vec<ContentDirectoryBrowseItem> {
    fn browse_direct_children(&self, _parent_id: usize) -> Vec<ContentDirectoryBrowseItem> {
        self.clone()
    }
}
