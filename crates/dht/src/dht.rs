use std::{
    f32::consts::E,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU16, Ordering},
        Arc,
    },
    task::Poll,
    time::{Duration, Instant},
};

use crate::{
    bprotocol::{
        self, CompactNodeInfo, CompactPeerInfo, ErrorDescription, FindNodeRequest, GetPeersRequest,
        Message, MessageKind, Node, PingRequest, Response,
    },
    routing_table::{InsertResult, RoutingTable},
    RESPONSE_TIMEOUT,
};
use anyhow::Context;
use backoff::{backoff::Backoff, ExponentialBackoffBuilder};
use bencode::{ByteBuf, ByteString};
use dashmap::DashMap;
use futures::{future::join_all, stream::FuturesUnordered, Stream, StreamExt, TryFutureExt};
use indexmap::IndexSet;
use leaky_bucket::RateLimiter;
use librqbit_core::{id20::Id20, peer_id::generate_peer_id, spawn_utils::spawn};
use parking_lot::RwLock;
use rand::Rng;
use serde::Serialize;
use tokio::{
    net::UdpSocket,
    sync::mpsc::{channel, unbounded_channel, Sender, UnboundedReceiver, UnboundedSender},
};
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use tracing::{debug, debug_span, error_span, info, trace, warn, Instrument};

#[derive(Debug, Serialize)]
pub struct DhtStats {
    #[serde(serialize_with = "crate::utils::serialize_id20")]
    pub id: Id20,
    pub outstanding_requests: usize,
    pub seen_peers: usize,
    pub made_requests: usize,
    pub routing_table_size: usize,
}

struct OutstandingRequest {
    done: tokio::sync::oneshot::Sender<ResponseOrError>,
}

pub struct DhtState {
    id: Id20,
    next_transaction_id: AtomicU16,

    // Created requests: (transaction_id, addr) => Requests.
    // If we get a response, it gets removed from here.
    inflight: DashMap<(u16, SocketAddr), OutstandingRequest>,

    // TODO: clean up old entries
    made_requests_by_addr: DashMap<(Request, SocketAddr), Instant>,

    routing_table: RwLock<RoutingTable>,
    listen_addr: SocketAddr,

    // Sending requests to the worker.
    sender: UnboundedSender<(Message<ByteString>, SocketAddr)>,

    seen_peers: DashMap<Id20, IndexSet<SocketAddr>>,
    get_peers_subscribers: DashMap<Id20, tokio::sync::broadcast::Sender<SocketAddr>>,
}

impl DhtState {
    fn new_internal(
        id: Id20,
        sender: UnboundedSender<(Message<ByteString>, SocketAddr)>,
        routing_table: Option<RoutingTable>,
        listen_addr: SocketAddr,
    ) -> Self {
        let routing_table = routing_table.unwrap_or_else(|| RoutingTable::new(id));
        Self {
            id,
            next_transaction_id: AtomicU16::new(0),
            inflight: Default::default(),
            routing_table: RwLock::new(routing_table),
            sender,
            listen_addr,
            seen_peers: Default::default(),
            get_peers_subscribers: Default::default(),
            made_requests_by_addr: Default::default(),
        }
    }

    fn spawn_request(self: &Arc<Self>, request: Request, addr: SocketAddr) {
        let this = self.clone();
        spawn(
            error_span!(parent: None, "dht_request", addr=addr.to_string(), request=format!("{:?}", request)),
            async move {
                match this.send_request_and_handle_response(request, addr).await {
                    Ok(_) => {}
                    Err(e) => {
                        debug!("error: {:?}", e);
                    }
                };
                Ok(())
            },
        );
    }

    async fn send_request_and_handle_response(
        self: &Arc<Self>,
        request: Request,
        addr: SocketAddr,
    ) -> anyhow::Result<()> {
        let resp = self.request(request, addr).await?;
        match resp {
            ResponseOrError::Response(r) => self.on_response(addr, request, r),
            ResponseOrError::Error(e) => {
                anyhow::bail!("received error: {:?}", e);
            }
        }
    }

    async fn request(&self, request: Request, addr: SocketAddr) -> anyhow::Result<ResponseOrError> {
        let (tid, msg) = self.create_request(request);
        let key = (tid, addr);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.inflight.insert(key, OutstandingRequest { done: tx });
        match self.sender.send((msg, addr)) {
            Ok(_) => {}
            Err(e) => {
                self.inflight.remove(&key);
                return Err(e.into());
            }
        };
        match tokio::time::timeout(RESPONSE_TIMEOUT, rx).await {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(e)) => {
                self.inflight.remove(&key);
                warn!("recv error, did not expect this: {:?}", e);
                Err(e.into())
            }
            Err(e) => {
                self.inflight.remove(&key);
                anyhow::bail!("timeout")
            }
        }
    }

    fn create_request(&self, request: Request) -> (u16, Message<ByteString>) {
        let transaction_id = self.next_transaction_id.fetch_add(1, Ordering::Relaxed);
        let transaction_id_buf = [(transaction_id >> 8) as u8, (transaction_id & 0xff) as u8];

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
            Request::Ping => Message {
                transaction_id: ByteString::from(transaction_id_buf.as_ref()),
                version: None,
                ip: None,
                kind: MessageKind::PingRequest(PingRequest { id: self.id }),
            },
        };
        (transaction_id, message)
    }

    fn on_response(
        self: &Arc<Self>,
        addr: SocketAddr,
        request: Request,
        response: Response<ByteString>,
    ) -> anyhow::Result<()> {
        match request {
            Request::FindNode(id) => {
                let nodes = response
                    .nodes
                    .ok_or_else(|| anyhow::anyhow!("expected nodes for find_node requests"))?;
                self.on_found_nodes(response.id, addr, id, nodes)
            }
            Request::GetPeers(id) => self.on_found_peers_or_nodes(response.id, addr, id, response),
            Request::Ping => Ok(()),
        }
    }

    fn on_incoming_from_remote(
        self: &Arc<Self>,
        msg: Message<ByteString>,
        addr: SocketAddr,
    ) -> anyhow::Result<()> {
        let generate_compact_nodes = |target| {
            let nodes = self
                .routing_table
                .read()
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
            // If it's a response to a request we made, find the request task, notify it with the response,
            // and let it handle it.
            MessageKind::Error(_) | MessageKind::Response(_) => {
                if msg.transaction_id.len() != 2 {
                    anyhow::bail!(
                        "{}: transaction id unrecognized, expected its length == 2. Message: {:?}",
                        addr,
                        msg
                    )
                }
                let tid = ((msg.transaction_id[0] as u16) << 8) + (msg.transaction_id[1] as u16);
                let request = match self.inflight.remove(&(tid, addr)).map(|(_, v)| v) {
                    Some(req) => req,
                    None => anyhow::bail!("outstanding request not found. Message: {:?}", msg),
                };

                let response_or_error = match msg.kind {
                    MessageKind::Error(e) => ResponseOrError::Error(e),
                    MessageKind::Response(r) => {
                        self.routing_table.write().mark_response(&r.id);
                        ResponseOrError::Response(r)
                    }
                    _ => unreachable!(),
                };
                match request.done.send(response_or_error) {
                    Ok(_) => {}
                    Err(e) => {
                        warn!(
                            "recieved response, but the receiver task is closed: {:?}",
                            e
                        );
                    }
                }
                Ok(())
            }
            // Otherwise, respond to a query.
            MessageKind::PingRequest(req) => {
                let message = Message {
                    transaction_id: msg.transaction_id,
                    version: None,
                    ip: None,
                    kind: MessageKind::Response(bprotocol::Response {
                        id: self.id,
                        ..Default::default()
                    }),
                };
                self.routing_table.write().mark_last_query(&req.id);
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
                self.routing_table.write().mark_last_query(&req.id);
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
                self.routing_table.write().mark_last_query(&req.id);
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
            outstanding_requests: self.inflight.len(),
            seen_peers: self.seen_peers.iter().map(|e| e.value().len()).sum(),
            made_requests: self.made_requests_by_addr.len(),
            routing_table_size: self.routing_table.read().len(),
        }
    }

    #[allow(clippy::type_complexity)]
    fn get_peers_internal(
        self: &Arc<Self>,
        info_hash: Id20,
    ) -> anyhow::Result<(
        Option<(usize, usize)>,
        tokio::sync::broadcast::Receiver<SocketAddr>,
    )> {
        use dashmap::mapref::entry::Entry;
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
                    .read()
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

    fn should_request(&self, request: Request, addr: SocketAddr) -> bool {
        const RE_REQUEST_TIME: Duration = Duration::from_secs(10 * 60);
        use dashmap::mapref::entry::Entry;
        match self.made_requests_by_addr.entry((request, addr)) {
            Entry::Occupied(mut o) => {
                if o.get().elapsed() > RE_REQUEST_TIME {
                    o.insert(Instant::now());
                    true
                } else {
                    false
                }
            }
            Entry::Vacant(v) => {
                v.insert(Instant::now());
                true
            }
        }
    }

    fn send_find_peers_if_not_yet(
        self: &Arc<Self>,
        info_hash: Id20,
        target_node: Id20,
        addr: SocketAddr,
    ) -> anyhow::Result<()> {
        let request = Request::GetPeers(info_hash);
        if self.should_request(request, addr) {
            self.routing_table
                .write()
                .mark_outgoing_request(&target_node);
            self.spawn_request(request, addr);
        }
        Ok(())
    }

    fn send_find_node_if_not_yet(
        self: &Arc<Self>,
        search_id: Id20,
        target_node: Id20,
        addr: SocketAddr,
    ) -> anyhow::Result<()> {
        let request = Request::FindNode(search_id);
        if self.should_request(request, addr) {
            self.routing_table
                .write()
                .mark_outgoing_request(&target_node);
            self.spawn_request(request, addr);
        }
        Ok(())
    }

    fn routing_table_add_node(self: &Arc<Self>, id: Id20, addr: SocketAddr) -> InsertResult {
        let mut questionable_nodes = Vec::new();
        let res = self.routing_table.write().add_node(id, addr, |addr| {
            questionable_nodes.push(addr);
            true
        });
        for addr in questionable_nodes {
            self.spawn_request(Request::Ping, addr);
        }
        res
    }

    fn on_found_nodes(
        self: &Arc<Self>,
        source: Id20,
        source_addr: SocketAddr,
        target: Id20,
        nodes: CompactNodeInfo,
    ) -> anyhow::Result<()> {
        // We don't need to allocate/collect here, but the borrow checker is not happy
        // otherwise when we iterate self.searching_for_peers and mutating self in the loop.
        let searching_for_peers = self
            .get_peers_subscribers
            .iter()
            .map(|e| *e.key())
            .collect::<Vec<_>>();

        // On newly discovered nodes, ask them for peers that we are interested in.
        match self.routing_table_add_node(source, source_addr) {
            InsertResult::ReplacedBad(_) | InsertResult::Added => {
                for info_hash in &searching_for_peers {
                    self.send_find_peers_if_not_yet(*info_hash, source, source_addr)?;
                }
            }
            _ => {}
        };
        for node in nodes.nodes {
            match self.routing_table_add_node(node.id, node.addr.into()) {
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
        self: &Arc<Self>,
        source: Id20,
        source_addr: SocketAddr,
        target: Id20,
        data: bprotocol::Response<ByteString>,
    ) -> anyhow::Result<()> {
        self.routing_table_add_node(source, source_addr);
        self.routing_table.write().mark_response(&source);

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
            let mut seen = self.seen_peers.entry(target).or_default();

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
                self.routing_table_add_node(node.id, node.addr.into());
                self.send_find_peers_if_not_yet(target, node.id, node.addr.into())?;
            }
        };
        Ok(())
    }
}

fn make_rate_limiter() -> RateLimiter {
    // TODO: move to configuration, i'm lazy.
    let dht_queries_per_second = std::env::var("DHT_QUERIES_PER_SECOND")
        .map(|v| v.parse().expect("couldn't parse DHT_QUERIES_PER_SECOND"))
        .unwrap_or(250usize);

    let per_100_ms = dht_queries_per_second / 10;

    RateLimiter::builder()
        .initial(per_100_ms)
        .max(dht_queries_per_second)
        .interval(Duration::from_millis(100))
        .fair(false)
        .refill(per_100_ms)
        .build()
}

async fn run_framer(
    socket: &UdpSocket,
    mut input_rx: UnboundedReceiver<(Message<ByteString>, SocketAddr)>,
    output_tx: Sender<(Message<ByteString>, SocketAddr)>,
) -> anyhow::Result<()> {
    let writer = async {
        let mut buf = Vec::new();
        let rate_limiter = make_rate_limiter();
        while let Some((msg, addr)) = input_rx.recv().await {
            let addr = match addr {
                SocketAddr::V4(v4) => v4,
                SocketAddr::V6(_) => continue,
            };
            rate_limiter.acquire_one().await;
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
    Ping,
}

#[derive(Debug)]
enum ResponseOrError {
    Response(Response<ByteString>),
    Error(ErrorDescription<ByteString>),
}

struct DhtWorker {
    socket: UdpSocket,
    peer_id: Id20,
    state: Arc<DhtState>,
}

impl DhtWorker {
    fn on_response(&self, msg: Message<ByteString>, addr: SocketAddr) -> anyhow::Result<()> {
        self.state.on_incoming_from_remote(msg, addr)
    }

    async fn bootstrap_one_ip_with_backoff(&self, addr: SocketAddr) -> anyhow::Result<()> {
        let mut backoff = ExponentialBackoffBuilder::new()
            .with_initial_interval(Duration::from_secs(10))
            .with_multiplier(1.5)
            .with_max_interval(Duration::from_secs(60))
            .with_max_elapsed_time(Some(Duration::from_secs(86400)))
            .build();

        loop {
            let res = self
                .state
                .send_request_and_handle_response(Request::FindNode(self.peer_id), addr)
                .await;
            match res {
                Ok(r) => return Ok(r),
                Err(e) => {
                    debug!("error: {:?}", e);
                    if let Some(backoff) = backoff.next_backoff() {
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                    anyhow::bail!("given up bootstrapping, timed out")
                }
            }
        }
    }

    async fn bootstrap_hostname(&self, hostname: &str) -> anyhow::Result<()> {
        let addrs = tokio::net::lookup_host(hostname)
            .await
            .with_context(|| format!("error looking up {}", hostname))?;
        let mut futs = FuturesUnordered::new();
        for addr in addrs {
            futs.push(
                self.bootstrap_one_ip_with_backoff(addr)
                    .instrument(error_span!("addr", addr = addr.to_string())),
            );
        }
        let requests = futs.len();
        let mut successes = 0;
        while let Some(resp) = futs.next().await {
            if resp.is_ok() {
                successes += 1
            };
        }
        if successes == 0 {
            anyhow::bail!("none of the {} bootstrap requests succeded", requests);
        }
        Ok(())
    }

    async fn bootstrap_hostname_with_backoff(&self, addr: &str) -> anyhow::Result<()> {
        let mut backoff = ExponentialBackoffBuilder::new()
            .with_initial_interval(Duration::from_secs(10))
            .with_multiplier(1.5)
            .with_max_interval(Duration::from_secs(60))
            .with_max_elapsed_time(Some(Duration::from_secs(86400)))
            .build();

        loop {
            let backoff = match self.bootstrap_hostname(addr).await {
                Ok(_) => return Ok(()),
                Err(e) => {
                    warn!("error: {}", e);
                    backoff.next_backoff()
                }
            };
            if let Some(backoff) = backoff {
                tokio::time::sleep(backoff).await;
                continue;
            }
            anyhow::bail!("bootstrap failed")
        }
    }

    async fn bootstrap(&self, bootstrap_addrs: &[String]) -> anyhow::Result<()> {
        let mut futs = FuturesUnordered::new();

        for addr in bootstrap_addrs.iter() {
            let this = &self;
            futs.push(
                this.bootstrap_hostname_with_backoff(addr)
                    .instrument(error_span!("bootstrap", hostname = addr)),
            );
        }
        let mut successes = 0;
        while let Some(resp) = futs.next().await {
            if resp.is_ok() {
                successes += 1
            }
        }
        if successes == 0 {
            anyhow::bail!("bootstrapping failed")
        }
        Ok(())
    }

    async fn start(
        self,
        in_rx: UnboundedReceiver<(Message<ByteString>, SocketAddr)>,
        bootstrap_addrs: &[String],
    ) -> anyhow::Result<()> {
        let (out_tx, mut out_rx) = channel(1);
        let framer = run_framer(&self.socket, in_rx, out_tx).instrument(debug_span!("dht_framer"));

        let bootstrap = self.bootstrap(bootstrap_addrs);
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
        }
        .instrument(debug_span!("dht_responese_reader"));

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
    state: Arc<DhtState>,
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

impl DhtState {
    pub async fn new() -> anyhow::Result<Arc<Self>> {
        Self::with_config(DhtConfig::default()).await
    }
    pub async fn with_config(config: DhtConfig) -> anyhow::Result<Arc<Self>> {
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
        let state = Arc::new(Self::new_internal(
            peer_id,
            in_tx,
            config.routing_table,
            listen_addr,
        ));

        spawn(error_span!("dht"), {
            let state = state.clone();
            async move {
                let worker = DhtWorker {
                    socket,
                    peer_id,
                    state,
                };
                worker.start(in_rx, &bootstrap_addrs).await?;
                Ok(())
            }
        });
        Ok(state)
    }

    pub fn get_peers(
        self: &Arc<Self>,
        info_hash: Id20,
    ) -> anyhow::Result<impl Stream<Item = SocketAddr> + Unpin> {
        let (pos, rx) = self.get_peers_internal(info_hash)?;
        Ok(PeerStream {
            info_hash,
            state: self.clone(),
            absolute_stream_pos: 0,
            initial_peers_pos: pos,
            broadcast_rx: BroadcastStream::new(rx),
        })
    }

    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    pub fn stats(&self) -> DhtStats {
        self.get_stats()
    }

    pub fn with_routing_table<R, F: FnOnce(&RoutingTable) -> R>(&self, f: F) -> R {
        f(&self.routing_table.read())
    }

    pub fn clone_routing_table(&self) -> RoutingTable {
        self.routing_table.read().clone()
    }
}
