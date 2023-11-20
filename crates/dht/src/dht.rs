use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    net::SocketAddr,
    sync::Arc,
    task::Poll,
};

use crate::{
    bprotocol::{
        self, CompactNodeInfo, CompactPeerInfo, FindNodeRequest, GetPeersRequest, Message,
        MessageKind, Node,
    },
    routing_table::{InsertResult, RoutingTable},
};
use anyhow::Context;
use bencode::ByteString;
use futures::{stream::FuturesUnordered, Stream, StreamExt};
use indexmap::IndexSet;
use librqbit_core::{id20::Id20, peer_id::generate_peer_id};
use parking_lot::RwLock;
use rand::Rng;
use serde::Serialize;
use tokio::{
    net::UdpSocket,
    sync::mpsc::{channel, unbounded_channel, Sender, UnboundedReceiver, UnboundedSender},
};
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use tracing::{debug, info, trace, warn};

#[derive(Debug, Serialize)]
pub struct DhtStats {
    #[serde(serialize_with = "crate::utils::serialize_id20")]
    pub id: Id20,
    pub outstanding_requests: usize,
    pub seen_peers: usize,
    pub made_requests: usize,
    pub routing_table_size: usize,
}

struct DhtState {
    id: Id20,
    next_transaction_id: u16,
    outstanding_requests: HashMap<(u16, SocketAddr), Request>,
    routing_table: RoutingTable,
    listen_addr: SocketAddr,

    // This sender sends requests to the worker.
    // It is unbounded so that the methods on Dht state don't need to be async.
    // If the methods on Dht state were async, we would have a problem, as it's behind
    // a lock.
    // Alternatively, we can lock only the parts that change, and use that internally inside DhtState...
    sender: UnboundedSender<(Message<ByteString>, SocketAddr)>,

    seen_peers: HashMap<Id20, IndexSet<SocketAddr>>,
    get_peers_subscribers: HashMap<Id20, tokio::sync::broadcast::Sender<SocketAddr>>,

    made_requests: HashSet<(Request, SocketAddr)>,
}

impl DhtState {
    fn new(
        id: Id20,
        sender: UnboundedSender<(Message<ByteString>, SocketAddr)>,
        routing_table: Option<RoutingTable>,
        listen_addr: SocketAddr,
    ) -> Self {
        let routing_table = routing_table.unwrap_or_else(|| RoutingTable::new(id));
        Self {
            id,
            next_transaction_id: 0,
            outstanding_requests: Default::default(),
            routing_table,
            sender,
            listen_addr,
            seen_peers: Default::default(),
            get_peers_subscribers: Default::default(),
            made_requests: Default::default(),
        }
    }

    fn create_request(&mut self, request: Request, addr: SocketAddr) -> Message<ByteString> {
        let transaction_id = self.next_transaction_id;
        let transaction_id_buf = [(transaction_id >> 8) as u8, (transaction_id & 0xff) as u8];

        self.next_transaction_id = if transaction_id == u16::MAX {
            0
        } else {
            transaction_id + 1
        };
        let message = match request {
            Request::GetPeers(info_hash) => Message {
                transaction_id: ByteString::from(transaction_id_buf.as_ref()),
                version: None,
                ip: None,
                kind: MessageKind::GetPeersRequest(GetPeersRequest {
                    id: self.id,
                    info_hash,
                }),
            },
            Request::FindNode(target) => Message {
                transaction_id: ByteString::from(transaction_id_buf.as_ref()),
                version: None,
                ip: None,
                kind: MessageKind::FindNodeRequest(FindNodeRequest {
                    id: self.id,
                    target,
                }),
            },
        };
        self.outstanding_requests
            .insert((transaction_id, addr), request);
        message
    }
    fn on_incoming_from_remote(
        &mut self,
        msg: Message<ByteString>,
        addr: SocketAddr,
    ) -> anyhow::Result<()> {
        let generate_compact_nodes = |target| {
            let nodes = self
                .routing_table
                .sorted_by_distance_from(target)
                .into_iter()
                .filter_map(|r| {
                    Some(Node {
                        id: r.id(),
                        addr: match r.addr() {
                            SocketAddr::V4(v4) => v4,
                            SocketAddr::V6(_) => return None,
                        },
                    })
                })
                .take(8)
                .collect::<Vec<_>>();
            CompactNodeInfo { nodes }
        };

        match &msg.kind {
            MessageKind::Error(_) | MessageKind::Response(_) => {
                if msg.transaction_id.len() != 2 {
                    anyhow::bail!(
                        "{}: transaction id unrecognized, expected its length == 2. Message: {:?}",
                        addr,
                        msg
                    )
                }
                let tid = ((msg.transaction_id[0] as u16) << 8) + (msg.transaction_id[1] as u16);
                let request = match self.outstanding_requests.remove(&(tid, addr)) {
                    Some(req) => req,
                    None => anyhow::bail!("outstanding request not found. Message: {:?}", msg),
                };
                let response = match msg.kind {
                    MessageKind::Error(e) => {
                        anyhow::bail!("request {:?} received error response {:?}", request, e)
                    }
                    MessageKind::Response(r) => r,
                    _ => unreachable!(),
                };
                self.routing_table.mark_response(&response.id);
                match request {
                    Request::FindNode(id) => {
                        let nodes = response.nodes.ok_or_else(|| {
                            anyhow::anyhow!("expected nodes for find_node requests")
                        })?;
                        self.on_found_nodes(response.id, addr, id, nodes)
                    }
                    Request::GetPeers(id) => {
                        self.on_found_peers_or_nodes(response.id, addr, id, response)
                    }
                }
            }
            MessageKind::PingRequest(_) => {
                let message = Message {
                    transaction_id: msg.transaction_id,
                    version: None,
                    ip: None,
                    kind: MessageKind::Response(bprotocol::Response {
                        id: self.id,
                        ..Default::default()
                    }),
                };
                self.sender.send((message, addr))?;
                Ok(())
            }
            MessageKind::GetPeersRequest(req) => {
                let peers = self.seen_peers.get(&req.info_hash).map(|peers| {
                    peers
                        .iter()
                        .copied()
                        .filter_map(|a| match a {
                            SocketAddr::V4(v4) => Some(CompactPeerInfo { addr: v4 }),
                            // this should never happen in practice
                            SocketAddr::V6(_) => None,
                        })
                        .take(50)
                        .collect::<Vec<_>>()
                });
                let token = if peers.is_some() {
                    let mut token = [0u8; 20];
                    rand::thread_rng().fill(&mut token);
                    Some(ByteString::from(token.as_ref()))
                } else {
                    None
                };
                let compact_node_info = generate_compact_nodes(req.info_hash);
                let message = Message {
                    transaction_id: msg.transaction_id,
                    version: None,
                    ip: None,
                    kind: MessageKind::Response(bprotocol::Response {
                        id: self.id,
                        nodes: Some(compact_node_info),
                        values: peers,
                        token,
                    }),
                };
                self.sender.send((message, addr))?;
                Ok(())
            }
            MessageKind::FindNodeRequest(req) => {
                let compact_node_info = generate_compact_nodes(req.target);
                let message = Message {
                    transaction_id: msg.transaction_id,
                    version: None,
                    ip: None,
                    kind: MessageKind::Response(bprotocol::Response {
                        id: self.id,
                        nodes: Some(compact_node_info),
                        ..Default::default()
                    }),
                };
                self.sender.send((message, addr))?;
                Ok(())
            }
        }
    }

    pub fn get_stats(&self) -> DhtStats {
        DhtStats {
            id: self.id,
            outstanding_requests: self.outstanding_requests.len(),
            seen_peers: self.seen_peers.values().map(|v| v.len()).sum(),
            made_requests: self.made_requests.len(),
            routing_table_size: self.routing_table.len(),
        }
    }

    #[allow(clippy::type_complexity)]
    fn get_peers(
        &mut self,
        info_hash: Id20,
    ) -> anyhow::Result<(
        Option<(usize, usize)>,
        tokio::sync::broadcast::Receiver<SocketAddr>,
    )> {
        match self.get_peers_subscribers.entry(info_hash) {
            Entry::Occupied(o) => {
                let pos = self.seen_peers.get(&info_hash).and_then(|p| {
                    if p.is_empty() {
                        None
                    } else {
                        Some((0, p.len()))
                    }
                });
                let rx = o.get().subscribe();
                Ok((pos, rx))
            }
            Entry::Vacant(v) => {
                // DHT sends peers REALLY fast, so ideally the consumer of this broadcast should not lag behind.
                // In case it does though we have PeerStream to replay.
                let (tx, rx) = tokio::sync::broadcast::channel(100);
                v.insert(tx);

                // We don't need to allocate/collect here, but the borrow checker is not happy otherwise.
                let nodes_to_query = self
                    .routing_table
                    .sorted_by_distance_from(info_hash)
                    .iter()
                    .map(|n| (n.id(), n.addr()))
                    .take(8)
                    .collect::<Vec<_>>();
                for (id, addr) in nodes_to_query {
                    self.send_find_peers_if_not_yet(info_hash, id, addr)?;
                }

                Ok((None, rx))
            }
        }
    }

    fn send_find_peers_if_not_yet(
        &mut self,
        info_hash: Id20,
        target_node: Id20,
        addr: SocketAddr,
    ) -> anyhow::Result<()> {
        let request = Request::GetPeers(info_hash);
        if self.made_requests.insert((request, addr)) {
            self.routing_table.mark_outgoing_request(&target_node);
            let msg = self.create_request(request, addr);
            self.sender.send((msg, addr))?;
        }
        Ok(())
    }

    fn send_find_node_if_not_yet(
        &mut self,
        search_id: Id20,
        target_node: Id20,
        addr: SocketAddr,
    ) -> anyhow::Result<()> {
        let request = Request::FindNode(search_id);
        if self.made_requests.insert((request, addr)) {
            self.routing_table.mark_outgoing_request(&target_node);
            let msg = self.create_request(request, addr);
            self.sender.send((msg, addr))?;
        }
        Ok(())
    }

    fn on_found_nodes(
        &mut self,
        source: Id20,
        source_addr: SocketAddr,
        target: Id20,
        nodes: CompactNodeInfo,
    ) -> anyhow::Result<()> {
        // We don't need to allocate/collect here, but the borrow checker is not happy
        // otherwise when we iterate self.searching_for_peers and mutating self in the loop.
        let searching_for_peers = self
            .get_peers_subscribers
            .keys()
            .copied()
            .collect::<Vec<_>>();

        // On newly discovered nodes, ask them for peers that we are interested in.
        match self.routing_table.add_node(source, source_addr) {
            InsertResult::ReplacedBad(_) | InsertResult::Added => {
                for info_hash in &searching_for_peers {
                    self.send_find_peers_if_not_yet(*info_hash, source, source_addr)?;
                }
            }
            _ => {}
        };
        for node in nodes.nodes {
            match self.routing_table.add_node(node.id, node.addr.into()) {
                InsertResult::ReplacedBad(_) | InsertResult::Added => {
                    for info_hash in &searching_for_peers {
                        self.send_find_peers_if_not_yet(*info_hash, node.id, node.addr.into())?;
                    }
                    // recursively find nodes closest to us until we can't find more.
                    self.send_find_node_if_not_yet(target, source, source_addr)?;
                }
                _ => {}
            };
        }
        Ok(())
    }

    fn on_found_peers_or_nodes(
        &mut self,
        source: Id20,
        source_addr: SocketAddr,
        target: Id20,
        data: bprotocol::Response<ByteString>,
    ) -> anyhow::Result<()> {
        self.routing_table.add_node(source, source_addr);
        self.routing_table.mark_response(&source);

        let bsender = match self.get_peers_subscribers.get(&target) {
            Some(s) => s,
            None => {
                warn!(
                    "ignoring get_peers response, no subscribers for {:?}",
                    target
                );
                return Ok(());
            }
        };

        if let Some(peers) = data.values {
            let seen = self.seen_peers.entry(target).or_default();

            for peer in peers.iter() {
                if peer.addr.port() < 1024 {
                    debug!("bad peer port, ignoring: {}", peer.addr);
                    continue;
                }
                let addr = SocketAddr::V4(peer.addr);
                if seen.insert(addr) {
                    bsender
                        .send(addr)
                        .context("error sending peers to subscribers")?;
                }
            }
        };
        if let Some(nodes) = data.nodes {
            for node in nodes.nodes {
                self.routing_table.add_node(node.id, node.addr.into());
                self.send_find_peers_if_not_yet(target, node.id, node.addr.into())?;
            }
        };
        Ok(())
    }
}

async fn run_framer(
    socket: &UdpSocket,
    mut input_rx: UnboundedReceiver<(Message<ByteString>, SocketAddr)>,
    output_tx: Sender<(Message<ByteString>, SocketAddr)>,
) -> anyhow::Result<()> {
    let writer = async {
        let mut buf = Vec::new();
        while let Some((msg, addr)) = input_rx.recv().await {
            let addr = match addr {
                SocketAddr::V4(v4) => v4,
                SocketAddr::V6(_) => continue,
            };
            trace!("{}: sending {:?}", addr, &msg);
            buf.clear();
            bprotocol::serialize_message(
                &mut buf,
                msg.transaction_id,
                msg.version,
                msg.ip,
                msg.kind,
            )
            .unwrap();
            if let Err(e) = socket.send_to(&buf, addr).await {
                warn!("could not send to {:?}: {}", addr, e)
            }
        }
        Err::<(), _>(anyhow::anyhow!(
            "DHT UDP socket writer over, nowhere to read messages from"
        ))
    };
    let reader = async {
        let mut buf = vec![0u8; 16384];
        loop {
            let (size, addr) = socket
                .recv_from(&mut buf)
                .await
                .context("error reading from UDP socket")?;
            match bprotocol::deserialize_message::<ByteString>(&buf[..size]) {
                Ok(msg) => {
                    trace!("{}: received {:?}", addr, &msg);
                    match output_tx.send((msg, addr)).await {
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                Err(e) => debug!("{}: error deserializing incoming message: {}", addr, e),
            }
        }
        Err::<(), _>(anyhow::anyhow!(
            "DHT UDP socket reader over, nowhere to send responses to"
        ))
    };
    let result = tokio::select! {
        err = writer => err,
        err = reader => err,
    };
    result.context("DHT UDP framer closed")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Request {
    GetPeers(Id20),
    FindNode(Id20),
}

#[derive(Clone)]
pub struct Dht {
    state: Arc<RwLock<DhtState>>,
}

struct DhtWorker {
    socket: UdpSocket,
    peer_id: Id20,
    state: Arc<RwLock<DhtState>>,
}

impl DhtWorker {
    fn on_response(&self, msg: Message<ByteString>, addr: SocketAddr) -> anyhow::Result<()> {
        self.state.write().on_incoming_from_remote(msg, addr)
    }

    async fn start(
        self,
        in_tx: UnboundedSender<(Message<ByteString>, SocketAddr)>,
        in_rx: UnboundedReceiver<(Message<ByteString>, SocketAddr)>,
        bootstrap_addrs: &[String],
    ) -> anyhow::Result<()> {
        let (out_tx, mut out_rx) = channel(1);
        let framer = run_framer(&self.socket, in_rx, out_tx);

        let bootstrap = async {
            let mut futs = FuturesUnordered::new();
            // bootstrap
            for addr in bootstrap_addrs.iter() {
                let this = &self;
                let in_tx = &in_tx;
                futs.push(async move {
                    match tokio::net::lookup_host(addr).await {
                        Ok(addrs) => {
                            for addr in addrs {
                                let request = this
                                    .state
                                    .write()
                                    .create_request(Request::FindNode(this.peer_id), addr);
                                in_tx.send((request, addr))?;
                            }
                        }
                        Err(e) => warn!("error looking up {}: {}", addr, e),
                    }
                    Ok::<_, anyhow::Error>(())
                });
            }
            let mut successes = 0;
            while let Some(resp) = futs.next().await {
                match resp {
                    Ok(_) => successes += 1,
                    Err(e) => warn!("error in one of the bootstrappers: {}", e),
                }
            }
            if successes == 0 {
                anyhow::bail!("bootstrapping did not succeed")
            }
            Ok(())
        };
        let mut bootstrap_done = false;

        let response_reader = {
            let this = &self;
            async move {
                while let Some((response, addr)) = out_rx.recv().await {
                    if let Err(e) = this.on_response(response, addr) {
                        debug!("error in on_response, addr={:?}: {}", addr, e)
                    }
                }
                Err::<(), _>(anyhow::anyhow!(
                    "closed response reader, nowhere to send results to, DHT closed"
                ))
            }
        };

        tokio::pin!(framer);
        tokio::pin!(bootstrap);
        tokio::pin!(response_reader);

        loop {
            tokio::select! {
                err = &mut framer => {
                    anyhow::bail!("framer quit: {:?}", err)
                },
                result = &mut bootstrap, if !bootstrap_done => {
                    bootstrap_done = true;
                    result?;
                },
                err = &mut response_reader => {anyhow::bail!("response reader quit: {:?}", err)}
            }
        }
    }
}

struct PeerStream {
    info_hash: Id20,
    state: Arc<RwLock<DhtState>>,
    absolute_stream_pos: usize,
    initial_peers_pos: Option<(usize, usize)>,
    broadcast_rx: BroadcastStream<SocketAddr>,
}

impl Stream for PeerStream {
    type Item = SocketAddr;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        loop {
            if let Some((pos, end)) = self.initial_peers_pos.take() {
                let addr = *self
                    .state
                    .read()
                    .seen_peers
                    .get(&self.info_hash)
                    .unwrap()
                    .get_index(pos)
                    .unwrap();
                if pos + 1 < end {
                    self.initial_peers_pos = Some((pos + 1, end));
                }
                self.absolute_stream_pos += 1;
                return Poll::Ready(Some(addr));
            }

            match self.broadcast_rx.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(v))) => {
                    self.absolute_stream_pos += 1;
                    return Poll::Ready(Some(v));
                }
                Poll::Ready(Some(Err(BroadcastStreamRecvError::Lagged(lagged_by)))) => {
                    debug!("peer stream is lagged by {}", lagged_by);
                    let s = self.absolute_stream_pos;
                    let e = s + lagged_by as usize;
                    self.initial_peers_pos = Some((s, e));
                    continue;
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            };
        }
    }
}

#[derive(Default)]
pub struct DhtConfig {
    pub peer_id: Option<Id20>,
    pub bootstrap_addrs: Option<Vec<String>>,
    pub routing_table: Option<RoutingTable>,
    pub listen_addr: Option<SocketAddr>,
}

impl Dht {
    pub async fn new() -> anyhow::Result<Self> {
        Self::with_config(DhtConfig::default()).await
    }
    pub async fn with_config(config: DhtConfig) -> anyhow::Result<Self> {
        let socket = match config.listen_addr {
            Some(addr) => UdpSocket::bind(addr)
                .await
                .with_context(|| format!("error binding socket, address {addr}")),
            None => UdpSocket::bind("0.0.0.0:0")
                .await
                .context("error binding socket, address 0.0.0.0:0"),
        }?;

        let listen_addr = socket
            .local_addr()
            .context("cannot determine UDP listen addr")?;
        info!("DHT listening on {:?}", listen_addr);

        let peer_id = config.peer_id.unwrap_or_else(generate_peer_id);
        info!("starting up DHT with peer id {:?}", peer_id);
        let bootstrap_addrs = config
            .bootstrap_addrs
            .unwrap_or_else(|| crate::DHT_BOOTSTRAP.iter().map(|v| v.to_string()).collect());

        let (in_tx, in_rx) = unbounded_channel();
        let state = Arc::new(RwLock::new(DhtState::new(
            peer_id,
            in_tx.clone(),
            config.routing_table,
            listen_addr,
        )));

        tokio::spawn({
            let state = state.clone();
            async move {
                let worker = DhtWorker {
                    socket,
                    peer_id,
                    state,
                };
                let result = worker.start(in_tx, in_rx, &bootstrap_addrs).await;
                warn!("DHT worker finished with {:?}", result);
            }
        });
        Ok(Dht { state })
    }
    pub async fn get_peers(
        &self,
        info_hash: Id20,
    ) -> anyhow::Result<impl Stream<Item = SocketAddr> + Unpin> {
        let (pos, rx) = self.state.write().get_peers(info_hash)?;
        Ok(PeerStream {
            info_hash,
            state: self.state.clone(),
            absolute_stream_pos: 0,
            initial_peers_pos: pos,
            broadcast_rx: BroadcastStream::new(rx),
        })
    }

    pub fn listen_addr(&self) -> SocketAddr {
        self.state.read().listen_addr
    }

    pub fn stats(&self) -> DhtStats {
        self.state.read().get_stats()
    }

    pub fn with_routing_table<R, F: FnOnce(&RoutingTable) -> R>(&self, f: F) -> R {
        f(&self.state.read().routing_table)
    }

    pub fn clone_routing_table(&self) -> RoutingTable {
        self.state.read().routing_table.clone()
    }
}
