use std::{collections::HashSet, net::SocketAddr, sync::Arc};

use tokio::sync::{Notify, Semaphore};

use crate::{torrent_state::InflightRequest, type_aliases::BF};

pub enum PeerState {
    Connecting(SocketAddr),
    Live(LivePeerState),
}

pub struct LivePeerState {
    #[allow(unused)]
    peer_id: [u8; 20],
    pub i_am_choked: bool,
    #[allow(unused)]
    pub peer_choked: bool,
    #[allow(unused)]
    pub peer_interested: bool,
    pub outstanding_requests: Arc<Semaphore>,
    pub have_notify: Arc<Notify>,
    pub bitfield: Option<BF>,
    pub inflight_requests: HashSet<InflightRequest>,
}

impl LivePeerState {
    pub fn new(peer_id: [u8; 20]) -> Self {
        LivePeerState {
            peer_id: peer_id,
            i_am_choked: true,
            peer_choked: true,
            peer_interested: false,
            bitfield: None,
            have_notify: Arc::new(Notify::new()),
            outstanding_requests: Arc::new(Semaphore::new(0)),
            inflight_requests: Default::default(),
        }
    }
}
