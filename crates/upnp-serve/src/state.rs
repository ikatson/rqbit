use std::sync::Arc;

use axum::body::Bytes;

use crate::upnp_types::content_directory::ContentDirectoryBrowseProvider;

pub struct UnpnServerStateInner {
    pub rendered_root_description: Bytes,
    pub provider: Box<dyn ContentDirectoryBrowseProvider>,
}

pub type UnpnServerState = Arc<UnpnServerStateInner>;
