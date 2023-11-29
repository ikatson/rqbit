use std::{
    cmp::Reverse,
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
    REQUERY_INTERVAL, RESPONSE_TIMEOUT,
};
use anyhow::{bail, Context};
use backoff::{backoff::Backoff, ExponentialBackoffBuilder};
use bencode::ByteString;
use dashmap::DashMap;
use futures::{future::BoxFuture, stream::FuturesUnordered, FutureExt, Stream, StreamExt};
use indexmap::IndexSet;
use leaky_bucket::RateLimiter;
use librqbit_core::{id20::Id20, peer_id::generate_peer_id, spawn_utils::spawn};
use parking_lot::RwLock;
use rand::Rng;
use serde::Serialize;
use tokio::{
    net::UdpSocket,
    sync::{
        mpsc::{channel, unbounded_channel, Sender, UnboundedReceiver, UnboundedSender},
        Notify,
    },
};
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use tracing::{debug, debug_span, error, error_span, info, trace, warn, Instrument};

#[derive(Debug, Serialize)]
pub struct DhtStats {
    #[serde(serialize_with = "crate::utils::serialize_id20")]
    pub id: Id20,
    pub outstanding_requests: usize,
    pub routing_table_size: usize,
}

struct OutstandingRequest {
    done: tokio::sync::oneshot::Sender<anyhow::Result<ResponseOrError>>,
}

pub struct WorkerSendRequest {
    our_tid: Option<u16>,
    message: Message<ByteString>,
    addr: SocketAddr,
}

#[derive(Debug)]
struct MaybeUsefulNode {
    id: Id20,
    addr: SocketAddr,
    last_request: Instant,
    last_response: Option<Instant>,
    returned_peers: bool,
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

struct RequestPeers {
    info_hash: Id20,
    dht: Arc<DhtState>,
    useful_nodes: RwLock<Vec<MaybeUsefulNode>>,
    tx: tokio::sync::mpsc::UnboundedSender<SocketAddr>,
}

struct RequestPeersStream {
    rx: tokio::sync::mpsc::UnboundedReceiver<SocketAddr>,
    cancel_join_handle: tokio::task::JoinHandle<()>,
}

impl RequestPeersStream {
    fn new(dht: Arc<DhtState>, info_hash: Id20) -> Self {
        let (tx, rx) = unbounded_channel();
        let rp = Arc::new(RequestPeers {
            info_hash,
            dht,
            useful_nodes: RwLock::new(Vec::new()),
            tx,
        });
        let join_handle = rp.request_peers_forever();
        Self {
            rx,
            cancel_join_handle: join_handle,
        }
    }
}

impl Drop for RequestPeersStream {
    fn drop(&mut self) {
        self.cancel_join_handle.abort();
    }
}

impl Stream for RequestPeersStream {
    type Item = SocketAddr;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

impl RequestPeers {
    fn request_peers_forever(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        spawn(
            error_span!("request_peers", info_hash = format!("{:?}", self.info_hash)),
            async move {
                let mut iteration = 0;
                loop {
                    debug!("iteration {}", iteration);
                    let sleep_duration = match self.get_peers_root().await {
                        Ok(_) => Duration::from_secs(60),
                        Err(e) => {
                            debug!("error: {e:?}");
                            Duration::from_secs(1)
                        }
                    };
                    tokio::time::sleep(sleep_duration).await;
                    iteration += 1;
                }
            },
        )
    }
    fn request_peers_one<'a>(
        self: &'a Arc<Self>,
        addr: SocketAddr,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        let fut = async move {
            let response = self
                .dht
                .request(Request::GetPeers(self.info_hash), addr)
                .await?;
            let response = match response {
                ResponseOrError::Response(r) => r,
                ResponseOrError::Error(e) => {
                    bail!("error response: {:?}", e)
                }
            };

            self.mark_node_responded(addr, &response);

            if let Some(peers) = response.values {
                for peer in peers {
                    self.tx.send(SocketAddr::V4(peer.addr))?;
                }
            }

            let mut futs = FuturesUnordered::new();
            if let Some(nodes) = response.nodes {
                for node in nodes.nodes {
                    let addr = SocketAddr::V4(node.addr);
                    if self.should_request_node(node.id, addr) {
                        futs.push(self.request_peers_one(addr));
                    }
                }
            }

            while let Some(res) = futs.next().await {
                if let Err(e) = res {
                    debug!("error: {e:?}")
                }
            }

            Ok(())
        };
        fut.boxed()
    }

    async fn get_peers_root(self: &Arc<Self>) -> anyhow::Result<()> {
        let mut futs = FuturesUnordered::new();
        for (_, addr) in self
            .dht
            .routing_table
            .read()
            .sorted_by_distance_from(self.info_hash)
            .iter()
            .map(|n| (n.id(), n.addr()))
            .take(8)
        {
            futs.push(self.request_peers_one(addr))
        }
        if futs.is_empty() {
            bail!("no nodes in routing table")
        }
        while let Some(res) = futs.next().await {
            if let Err(e) = res {
                debug!("error: {e:?}")
            }
        }
        Ok(())
    }

    fn mark_node_responded(&self, addr: SocketAddr, response: &Response<ByteString>) {
        let mut closest_nodes = self.useful_nodes.write();
        for node in closest_nodes.iter_mut() {
            if node.addr == addr {
                node.last_response = Some(Instant::now());
                node.returned_peers = response
                    .values
                    .as_ref()
                    .map(|c| !c.is_empty())
                    .unwrap_or(false);
                break;
            }
        }
    }

    fn should_request_node(&self, node_id: Id20, addr: SocketAddr) -> bool {
        let mut closest_nodes = self.useful_nodes.write();

        // If recently requested, ignore
        if let Some(existing) = closest_nodes.iter_mut().find(|n| n.id == node_id) {
            if existing.last_request.elapsed() > Duration::from_secs(60) {
                existing.last_request = Instant::now();
                return true;
            }
            return false;
        }

        closest_nodes.push(MaybeUsefulNode {
            id: node_id,
            addr,
            last_request: Instant::now(),
            last_response: None,
            returned_peers: false,
        });

        const LIMIT: usize = 256;
        closest_nodes.sort_by_key(|n| {
            let has_returned_peers_desc = Reverse(n.returned_peers);
            let has_responded_desc = Reverse(n.last_response.is_some() as u8);
            let distance = n.id.distance(&self.info_hash);
            (has_returned_peers_desc, has_responded_desc, distance)
        });
        if closest_nodes.len() > LIMIT {
            let popped = closest_nodes.pop().unwrap();
            if popped.id == node_id {
                return false;
            }
        }
        true
    }
}

pub struct DhtState {
    id: Id20,
    next_transaction_id: AtomicU16,
    bootstrapped: Notify,

    // Created requests: (transaction_id, addr) => Requests.
    // If we get a response, it gets removed from here.
    inflight_by_transaction_id: DashMap<(u16, SocketAddr), OutstandingRequest>,

    routing_table: RwLock<RoutingTable>,
    listen_addr: SocketAddr,

    // Sending requests to the worker.
    rate_limiter: RateLimiter,
    sender: UnboundedSender<WorkerSendRequest>,
}

impl DhtState {
    fn new_internal(
        id: Id20,
        sender: UnboundedSender<WorkerSendRequest>,
        routing_table: Option<RoutingTable>,
        listen_addr: SocketAddr,
    ) -> Self {
        let routing_table = routing_table.unwrap_or_else(|| RoutingTable::new(id));
        Self {
            id,
            bootstrapped: Default::default(),
            next_transaction_id: AtomicU16::new(0),
            inflight_by_transaction_id: Default::default(),
            routing_table: RwLock::new(routing_table),
            sender,
            listen_addr,
            rate_limiter: make_rate_limiter(),
        }
    }

    fn spawn_request(self: &Arc<Self>, request: Request, addr: SocketAddr) {
        let this = self.clone();
        spawn(
            error_span!(parent: None, "dht_spawn_request", addr=addr.to_string(), request=format!("{:?}", request)),
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
                bail!("received error: {:?}", e);
            }
        }
    }

    async fn request(&self, request: Request, addr: SocketAddr) -> anyhow::Result<ResponseOrError> {
        self.rate_limiter.acquire_one().await;
        let (tid, message) = self.create_request(request);
        let key = (tid, addr);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.inflight_by_transaction_id
            .insert(key, OutstandingRequest { done: tx });
        match self.sender.send(WorkerSendRequest {
            our_tid: Some(tid),
            message,
            addr,
        }) {
            Ok(_) => {}
            Err(e) => {
                self.inflight_by_transaction_id.remove(&key);
                return Err(e.into());
            }
        };
        match tokio::time::timeout(RESPONSE_TIMEOUT, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                self.inflight_by_transaction_id.remove(&key);
                warn!("recv error, did not expect this: {:?}", e);
                Err(e.into())
            }
            Err(_) => {
                self.inflight_by_transaction_id.remove(&key);
                bail!("timeout")
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
        self.routing_table.write().mark_response(&response.id);
        match request {
            Request::FindNode(id) => {
                let nodes = response
                    .nodes
                    .ok_or_else(|| anyhow::anyhow!("expected nodes for find_node requests"))?;
                self.on_found_nodes(response.id, addr, id, nodes)
            }
            Request::Ping => Ok(()),
            Request::GetPeers(info_hash) => {
                todo!()
                //                self.on_found_peers_or_nodes(response.id, addr, info_hash, response)
            }
        }
    }

    fn on_received_message(
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
                let tid = msg.get_our_transaction_id().context("bad transaction id")?;
                let request = match self
                    .inflight_by_transaction_id
                    .remove(&(tid, addr))
                    .map(|(_, v)| v)
                {
                    Some(req) => req,
                    None => bail!("outstanding request not found. Message: {:?}", msg),
                };

                let response_or_error = match msg.kind {
                    MessageKind::Error(e) => ResponseOrError::Error(e),
                    MessageKind::Response(r) => ResponseOrError::Response(r),
                    _ => unreachable!(),
                };
                match request.done.send(Ok(response_or_error)) {
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
                self.sender.send(WorkerSendRequest {
                    our_tid: None,
                    message,
                    addr,
                })?;
                Ok(())
            }
            MessageKind::GetPeersRequest(req) => {
                // let peers = self.info_hash_meta.get(&req.info_hash).map(|meta| {
                //     meta.seen_peers
                //         .iter()
                //         .copied()
                //         .filter_map(|a| match a {
                //             SocketAddr::V4(v4) => Some(CompactPeerInfo { addr: v4 }),
                //             // this should never happen in practice
                //             SocketAddr::V6(_) => None,
                //         })
                //         .take(50)
                //         .collect::<Vec<_>>()
                // });
                // let token = if peers.is_some() {
                //     let mut token = [0u8; 20];
                //     rand::thread_rng().fill(&mut token);
                //     Some(ByteString::from(token.as_ref()))
                // } else {
                //     None
                // };
                // let compact_node_info = generate_compact_nodes(req.info_hash);
                self.routing_table.write().mark_last_query(&req.id);
                let message = Message {
                    transaction_id: msg.transaction_id,
                    version: None,
                    ip: None,
                    kind: MessageKind::Response(bprotocol::Response {
                        id: self.id,
                        nodes: None,
                        values: None,
                        token: None,
                    }),
                };
                self.sender.send(WorkerSendRequest {
                    our_tid: None,
                    message,
                    addr,
                })?;
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
                self.sender.send(WorkerSendRequest {
                    our_tid: None,
                    message,
                    addr,
                })?;
                Ok(())
            }
        }
    }

    pub fn get_stats(&self) -> DhtStats {
        DhtStats {
            id: self.id,
            outstanding_requests: self.inflight_by_transaction_id.len(),
            routing_table_size: self.routing_table.read().len(),
        }
    }

    fn send_request_if_not_yet(
        self: &Arc<Self>,
        target_node: Id20,
        request: Request,
        addr: SocketAddr,
    ) -> anyhow::Result<()> {
        let this = self.clone();
        let fut = async move {
            this.routing_table
                .write()
                .mark_outgoing_request(&target_node);

            let resp = this.request(request, addr).await;
            match resp {
                Ok(ResponseOrError::Response(response)) => {
                    this.routing_table.write().mark_response(&target_node);
                    match this.on_response(addr, request, response) {
                        Ok(()) => {}
                        Err(e) => {
                            warn!("error in on_response: {:?}", e);
                        }
                    }
                }
                Ok(ResponseOrError::Error(e)) => {
                    this.routing_table.write().mark_response(&target_node);
                    debug!("error response: {:?}", e);
                }
                Err(e) => {
                    this.routing_table.write().mark_error(&target_node);
                    debug!("error: {:?}", e);
                }
            };
            Ok(())
        };

        spawn(
            error_span!(
                parent: None,
                "dht_request",
                addr = addr.to_string(),
                request = format!("{:?}", request),
            ),
            fut,
        );
        Ok(())
    }

    fn send_find_node_if_not_yet(
        self: &Arc<Self>,
        search_id: Id20,
        target_node: Id20,
        addr: SocketAddr,
    ) -> anyhow::Result<()> {
        self.send_request_if_not_yet(target_node, Request::FindNode(search_id), addr)
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
        self.routing_table_add_node(source, source_addr);
        for node in nodes.nodes {
            match self.routing_table_add_node(node.id, node.addr.into()) {
                InsertResult::ReplacedBad(_) | InsertResult::Added => {
                    // recursively find nodes closest to us until we can't find more.
                    self.send_find_node_if_not_yet(target, source, source_addr)?;
                }
                _ => {}
            };
        }
        Ok(())
    }
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
    fn on_send_error(&self, tid: u16, addr: SocketAddr, err: anyhow::Error) {
        if let Some((_, OutstandingRequest { done })) =
            self.state.inflight_by_transaction_id.remove(&(tid, addr))
        {
            let _ = done.send(Err(err)).is_err();
        };
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
                    bail!("given up bootstrapping, timed out")
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
            bail!("none of the {} bootstrap requests succeded", requests);
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
            bail!("bootstrap failed")
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
            bail!("bootstrapping failed")
        }
        Ok(())
    }

    async fn framer(
        &self,
        socket: &UdpSocket,
        mut input_rx: UnboundedReceiver<WorkerSendRequest>,
        output_tx: Sender<(Message<ByteString>, SocketAddr)>,
    ) -> anyhow::Result<()> {
        let writer = async {
            let mut buf = Vec::new();
            while let Some(WorkerSendRequest {
                our_tid,
                message,
                addr,
            }) = input_rx.recv().await
            {
                trace!("{}: sending {:?}", addr, &message);
                buf.clear();
                bprotocol::serialize_message(
                    &mut buf,
                    message.transaction_id,
                    message.version,
                    message.ip,
                    message.kind,
                )
                .unwrap();
                if let Err(e) = socket.send_to(&buf, addr).await {
                    debug!("error sending to {addr}: {e:?}");
                    if let Some(tid) = our_tid {
                        self.on_send_error(tid, addr, e.into());
                    }
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

    async fn start(
        self,
        in_rx: UnboundedReceiver<WorkerSendRequest>,
        bootstrap_addrs: &[String],
    ) -> anyhow::Result<()> {
        let (out_tx, mut out_rx) = channel(1);
        let framer = self
            .framer(&self.socket, in_rx, out_tx)
            .instrument(debug_span!("dht_framer"));

        let bootstrap = self.bootstrap(bootstrap_addrs);
        let mut bootstrap_done = false;

        let response_reader = {
            let this = &self;
            async move {
                while let Some((response, addr)) = out_rx.recv().await {
                    if let Err(e) = this.state.on_received_message(response, addr) {
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
}

impl Stream for PeerStream {
    type Item = SocketAddr;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        todo!()
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
        Ok(RequestPeersStream::new(self.clone(), info_hash))
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
