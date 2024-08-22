use std::sync::Arc;

use axum::body::Bytes;

pub struct UnpnServerStateInner {
    pub usn: String,
    pub friendly_name: String,
    pub server_header_string: String,
    pub port: u16,
    pub rendered_root_description: Bytes,
}

pub type UnpnServerState = Arc<UnpnServerStateInner>;
