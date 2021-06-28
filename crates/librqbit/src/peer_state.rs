use std::{collections::HashSet, net::SocketAddr, sync::Arc};

use tokio::sync::{Notify, Semaphore};

use crate::{torrent_state::InflightRequest, type_aliases::BF};

pub enum PeerState {
    Connecting(SocketAddr),
    Live(LivePeerState),
}

pub struct LivePeerState {
    #[allow(unused)]
    pub peer_id: [u8; 20],
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
