use std::{
    cmp::Reverse,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::Poll,
    time::{Duration, Instant},
};

use crate::{
    Error, INACTIVITY_TIMEOUT, REQUERY_INTERVAL, RESPONSE_TIMEOUT,
    bep42::{self, ExternalIpVoter},
    bep44::{self, ItemStore, MutableItem, StoredView, WriteTokens},
    bprotocol::{
        self, AnnouncePeer, CompactNodeInfo, CompactNodeInfoOwned, ErrorDescription,
        FindNodeRequest, FixedBytes, GetPeersRequest, GetRequest, Message, MessageKind, Node,
        PingRequest, PutRequest, RawBencode, Response, Want,
    },
    peer_store::PeerStore,
    routing_table::{InsertResult, NodeStatus, RoutingTable},
};
use backon::{ExponentialBuilder, Retryable};
use bencode::ByteBufOwned;
use bytes::Bytes;
use dashmap::DashMap;
use futures::{
    FutureExt, Stream, StreamExt, TryFutureExt, future::BoxFuture, stream::FuturesUnordered,
};

use leaky_bucket::RateLimiter;
use librqbit_core::{
    compact_ip::{CompactSerialize, CompactSerializeFixedLen},
    crate_version,
    hash_id::Id20,
    peer_id::generate_azereus_style,
    spawn_utils::{spawn, spawn_with_cancel},
};
use librqbit_dualstack_sockets::{BindDevice, UdpSocket};
use parking_lot::{Mutex, RwLock};

use serde::Serialize;
use tokio::sync::mpsc::{Sender, UnboundedReceiver, UnboundedSender, channel, unbounded_channel};

use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, debug_span, error, info, trace, warn};

fn now() -> Instant {
    Instant::now()
}

#[derive(Debug, Serialize)]
pub struct DhtStats {
    /// Our node id in the IPv4 DHT.
    #[serde(serialize_with = "crate::utils::serialize_id20")]
    pub id: Id20,
    /// Our node id in the IPv6 DHT. It follows our external IPv6 address once one is adopted.
    #[serde(serialize_with = "crate::utils::serialize_id20")]
    pub id_v6: Id20,
    pub outstanding_requests: usize,
    pub routing_table_size: usize,
    pub routing_table_size_v6: usize,
    /// Our external IPv4 address, as the DHT nodes we talk to agree on (BEP 42).
    pub external_ip_v4: Option<Ipv4Addr>,
    /// Our external IPv6 address, as the DHT nodes we talk to agree on (BEP 42).
    pub external_ip_v6: Option<Ipv6Addr>,
    /// How many BEP 44 items other nodes have stored with us.
    pub stored_items: usize,
}

/// Our node ids. BEP 42 ties a node id to the external address of the node, and we have one
/// external address per address family, so the IPv4 and the IPv6 DHT each get their own id.
#[derive(Debug, Clone, Copy)]
struct NodeIds {
    v4: Id20,
    v6: Id20,
}

impl NodeIds {
    fn get(&self, is_v4: bool) -> Id20 {
        if is_v4 { self.v4 } else { self.v6 }
    }
}

struct OutstandingRequest {
    done: tokio::sync::oneshot::Sender<crate::Result<ResponseOrError>>,
    // The node that pointed us to the address we asked (see `NodeToQuery::referrer`).
    referrer: Option<SocketAddr>,
    // Tells this request apart from a later one that happens to get the same key.
    serial: u64,
}

/// Removes the in-flight entry of a request when the request future goes away, whether it got
/// its answer, timed out or was dropped half-way (a lookup past its deadline, a cancelled task).
struct InflightGuard<'a> {
    map: &'a DashMap<(u16, SocketAddr), OutstandingRequest>,
    key: (u16, SocketAddr),
    serial: u64,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.map
            .remove_if(&self.key, |_, r| r.serial == self.serial);
    }
}

/// Transaction ids are random, so that a node that sees our queries cannot guess the ids of the
/// queries we send to others and forge their answers (and with them, votes on our address).
fn random_transaction_id() -> u16 {
    rand::random()
}

pub struct WorkerSendRequest {
    // If this is set, we are tracking the response in inflight_by_transaction_id
    our_tid: Option<u16>,
    message: Message<ByteBufOwned>,
    addr: SocketAddr,
}

#[derive(Debug)]
struct MaybeUsefulNode {
    id: Id20,
    addr: SocketAddr,
    last_request: Instant,
    last_response: Option<Instant>,
    errors_in_a_row: usize,
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

trait RecursiveRequestCallbacks: Sized + Send + Sync + 'static {
    fn on_request_start(&self, req: &RecursiveRequest<Self>, target_node: Id20, addr: SocketAddr);
    fn on_request_end(
        &self,
        req: &RecursiveRequest<Self>,
        target_node: Id20,
        addr: SocketAddr,
        resp: &crate::Result<ResponseOrError>,
    );
}

struct RecursiveRequestCallbacksGetPeers {
    // Id20::from_str("00000fffffffffffffffffffffffffffffffffff").unwrap()
    min_distance_to_announce: Id20,
    announce_port: Option<u16>,
}

impl RecursiveRequestCallbacks for RecursiveRequestCallbacksGetPeers {
    fn on_request_start(&self, _: &RecursiveRequest<Self>, _: Id20, _: SocketAddr) {}

    fn on_request_end(
        &self,
        req: &RecursiveRequest<Self>,
        target_node: Id20,
        addr: SocketAddr,
        resp: &crate::Result<ResponseOrError>,
    ) {
        let announce_port = match self.announce_port {
            Some(a) => a,
            None => return,
        };
        let resp = match resp {
            Ok(ResponseOrError::Response(resp)) => resp,
            _ => return,
        };
        let token = match &resp.token {
            Some(token) => token,
            None => return,
        };
        // BEP 42: with enforcement on, the token of a node whose id does not match its address
        // does not count, so it never becomes a storage target. The distance is the one of the
        // id the node answered with, the id that was checked.
        if !req
            .dht
            .storage_target_acceptable(target_node, resp.id, addr)
        {
            trace!(?addr, id = ?resp.id, listed = ?target_node, "not announcing to a node with a non-BEP 42 id");
            return;
        }
        if req.info_hash.distance(&resp.id) > self.min_distance_to_announce {
            trace!(
                "not announcing, {:?} is too far from {:?}",
                resp.id, req.info_hash
            );
            return;
        }
        let message = req.dht.create_request(
            random_transaction_id(),
            Request::Announce {
                info_hash: req.info_hash,
                token: token.clone(),
                port: announce_port,
            },
            addr,
        );

        // Nobody waits for the answer, so the request is not tracked.
        let _ = req.dht.worker_sender.send(WorkerSendRequest {
            our_tid: None,
            message,
            addr,
        });
    }
}

struct RecursiveRequestCallbacksFindNodes {}
impl RecursiveRequestCallbacks for RecursiveRequestCallbacksFindNodes {
    fn on_request_start(&self, req: &RecursiveRequest<Self>, target_node: Id20, addr: SocketAddr) {
        // The node can still help this lookup, but it does not go into the routing table.
        if !req.dht.node_id_acceptable(&target_node, addr) {
            trace!(?addr, id = ?target_node, "not adding a node with a non-BEP 42 id");
            return;
        }
        let mut rt = req.dht.get_table_for_addr(addr).write();
        match rt.add_node(target_node, addr) {
            InsertResult::WasExisting | InsertResult::ReplacedBad(_) | InsertResult::Added => {
                rt.mark_outgoing_request(&target_node, now());
            }
            InsertResult::Ignored => {}
        }
    }

    fn on_request_end(
        &self,
        req: &RecursiveRequest<Self>,
        target_node: Id20,
        addr: SocketAddr,
        resp: &crate::Result<ResponseOrError>,
    ) {
        let mut table = req.dht.get_table_for_addr(addr).write();
        if resp.is_ok() {
            table.mark_response(&target_node, now());
        } else {
            table.mark_error(&target_node);
        }
    }
}

struct RecursiveRequest<C: RecursiveRequestCallbacks> {
    max_depth: usize,
    useful_nodes_limit: usize,
    info_hash: Id20,
    request: Request,
    dht: Arc<DhtState>,
    useful_nodes: RwLock<Vec<MaybeUsefulNode>>,
    peer_tx: tokio::sync::mpsc::UnboundedSender<SocketAddr>,
    node_tx: tokio::sync::mpsc::UnboundedSender<NodeToQuery>,
    callbacks: C,
}

/// A node a lookup is going to query.
struct NodeToQuery {
    /// Its id, when we know it (bootstrap addresses come without one).
    id: Option<Id20>,
    addr: SocketAddr,
    /// The node whose answer listed it; `None` when it came from our routing table or the
    /// caller. BEP 42 votes on our external address count once per referrer, so that one node
    /// cannot vote through the many nodes it points us to.
    referrer: Option<SocketAddr>,
    depth: usize,
}

/// The most nodes one answer may add to a lookup: one bucket's worth, what an answer is
/// supposed to carry per address family. Anything beyond it is dropped, so that a hostile
/// answer cannot make us fire off hundreds of queries.
const MAX_NODES_PER_ANSWER: usize = 8;

pub struct RequestPeersStream {
    rx: tokio::sync::mpsc::UnboundedReceiver<SocketAddr>,
    cancel_join_handle_v4: tokio::task::JoinHandle<()>,
    cancel_join_handle_v6: tokio::task::JoinHandle<()>,
}

impl RequestPeersStream {
    fn new(dht: Arc<DhtState>, info_hash: Id20, announce_port: Option<u16>) -> Self {
        let (peer_tx, peer_rx) = unbounded_channel();
        let make = |is_v4: bool, dht: Arc<DhtState>, peer_tx: UnboundedSender<SocketAddr>| {
            let (node_tx, node_rx) = unbounded_channel();
            let rp = Arc::new(RecursiveRequest {
                max_depth: 4,
                info_hash,
                useful_nodes_limit: 256,
                request: Request::GetPeers(info_hash),
                dht,
                useful_nodes: RwLock::new(Vec::new()),
                peer_tx,
                node_tx,
                callbacks: RecursiveRequestCallbacksGetPeers {
                    min_distance_to_announce: Id20::from_str(
                        "0000ffffffffffffffffffffffffffffffffffff",
                    )
                    .unwrap(),
                    announce_port,
                },
            });
            rp.request_peers_forever(node_rx, is_v4)
        };

        let v4 = make(true, dht.clone(), peer_tx.clone());
        let v6 = make(false, dht, peer_tx);

        Self {
            rx: peer_rx,
            cancel_join_handle_v4: v4,
            cancel_join_handle_v6: v6,
        }
    }
}

impl Drop for RequestPeersStream {
    fn drop(&mut self) {
        self.cancel_join_handle_v4.abort();
        self.cancel_join_handle_v6.abort();
    }
}

impl Stream for RequestPeersStream {
    type Item = SocketAddr;

    #[inline(never)]
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

impl RecursiveRequest<RecursiveRequestCallbacksFindNodes> {
    async fn find_node_for_routing_table(
        dht: Arc<DhtState>,
        target: Id20,
        addrs: impl Iterator<Item = SocketAddr>,
    ) -> crate::Result<()> {
        let (node_tx, node_rx) = unbounded_channel();
        let req = RecursiveRequest {
            max_depth: 4,
            info_hash: target,
            request: Request::FindNode(target),
            dht,
            useful_nodes_limit: 32,
            useful_nodes: RwLock::new(Vec::new()),
            peer_tx: unbounded_channel().0,
            node_tx,
            callbacks: RecursiveRequestCallbacksFindNodes {},
        };
        req.run_to_completion(node_rx, addrs.map(|addr| (None, addr)))
            .await
    }
}

impl<C: RecursiveRequestCallbacks> RecursiveRequest<C> {
    /// Queries `seeds`, then every node they (and the nodes after them) point to, until no node
    /// is left to ask.
    async fn run_to_completion(
        &self,
        mut node_rx: UnboundedReceiver<NodeToQuery>,
        seeds: impl Iterator<Item = (Option<Id20>, SocketAddr)>,
    ) -> crate::Result<()> {
        let target = self.info_hash;
        let request_one = |node: NodeToQuery| {
            let addr = node.addr;
            self.request_one(node)
                .map_err(|e| {
                    debug!("error: {e:#}");
                    e
                })
                .instrument(debug_span!(
                    "find_node",
                    target = format!("{target:?}"),
                    addr = addr.to_string()
                ))
        };

        let mut futs = FuturesUnordered::new();

        let mut initial_addrs = 0;
        for (id, addr) in seeds {
            futs.push(request_one(NodeToQuery {
                id,
                addr,
                referrer: None,
                depth: 0,
            }));
            initial_addrs += 1;
        }

        let mut successes = 0;
        let mut errors = 0;

        loop {
            tokio::select! {
                biased;

                r = node_rx.recv() => {
                    // `self` holds the sender, so the channel never closes while we run.
                    let Some(node) = r else { break };
                    futs.push(request_one(node))
                },
                f = futs.next() => {
                    let f = match f {
                        Some(f) => f,
                        None => {
                            // The recursion finished.
                            break;
                        }
                    };
                    if f.is_ok() {
                        successes += 1;
                    } else {
                        errors += 1;
                    }
                }
            }
        }
        if successes == 0 {
            return Err(Error::NoSuccessfulLookups { errors });
        }
        debug!(
            "finished, successes = {successes}, errors = {errors}, initial_addrs = {initial_addrs}"
        );
        Ok(())
    }
}

/// How many of the closest nodes that gave us a write token a BEP 44 item is put to.
const PUT_TARGETS: usize = 8;
/// How long a BEP 44 lookup may take. When it runs out, what was found so far is used.
const ITEM_LOOKUP_DEADLINE: Duration = Duration::from_secs(30);

/// What a BEP 44 `get` lookup is after, which tells how to check the answers.
enum ItemQuery {
    Immutable,
    Mutable { public_key: [u8; 32], salt: Bytes },
}

/// A node that answered a `get` with a write token: somewhere to `put` the item.
struct PutTarget {
    distance: Id20,
    addr: SocketAddr,
    token: ByteBufOwned,
    // Whether it answered with an item (any version). BEP 44 sends `cas` only to those.
    has_item: bool,
}

#[derive(Default)]
struct ItemLookupFound {
    // The closest nodes that gave us a token, closest first, at most `PUT_TARGETS`.
    put_targets: Vec<PutTarget>,
    immutable: Option<Bytes>,
    mutable: Option<MutableItem>,
}

impl ItemLookupFound {
    fn add_put_target(&mut self, target: PutTarget) {
        if self.put_targets.iter().any(|t| t.addr == target.addr) {
            return;
        }
        self.put_targets.push(target);
        self.put_targets.sort_by_key(|t| t.distance);
        self.put_targets.truncate(PUT_TARGETS);
    }
}

/// Collects what the nodes of a BEP 44 `get` lookup answer: write tokens, and the item, checked
/// against the target (immutable) or the signature (mutable). Answers that do not check out are
/// ignored.
struct RecursiveRequestCallbacksGetItem {
    query: ItemQuery,
    found: Mutex<ItemLookupFound>,
    // Signalled when a checked immutable value arrives. It is final, so a plain `get` stops
    // there instead of waiting for the slowest node of the lookup.
    immutable_found: tokio::sync::Notify,
}

impl RecursiveRequestCallbacks for RecursiveRequestCallbacksGetItem {
    fn on_request_start(&self, _: &RecursiveRequest<Self>, _: Id20, _: SocketAddr) {}

    fn on_request_end(
        &self,
        req: &RecursiveRequest<Self>,
        target_node: Id20,
        addr: SocketAddr,
        resp: &crate::Result<ResponseOrError>,
    ) {
        let Ok(ResponseOrError::Response(resp)) = resp else {
            return;
        };
        let target = req.info_hash;
        let mut found = self.found.lock();
        // BEP 42: with enforcement on, a node whose id does not match its address is not a
        // storage target, as for announces.
        if let Some(token) = &resp.token
            && req
                .dht
                .storage_target_acceptable(target_node, resp.id, addr)
        {
            found.add_put_target(PutTarget {
                distance: resp.id.distance(&target),
                addr,
                token: token.clone(),
                has_item: resp.v.is_some() || resp.seq.is_some(),
            });
        }
        let Some(v) = &resp.v else {
            return;
        };
        let value = v.0.0.clone();
        match &self.query {
            ItemQuery::Immutable => {
                if bep44::immutable_target(&value) != target {
                    debug!(
                        ?addr,
                        "ignoring an immutable item that does not hash to its target"
                    );
                    return;
                }
                found.immutable.get_or_insert(value);
                self.immutable_found.notify_one();
            }
            ItemQuery::Mutable { public_key, salt } => {
                let (Some(k), Some(sig), Some(seq)) = (&resp.k, &resp.sig, resp.seq) else {
                    debug!(?addr, "ignoring a mutable item without k, sig or seq");
                    return;
                };
                if k.0 != *public_key
                    || !bep44::verify_mutable(public_key, salt, seq, &value, &sig.0)
                {
                    debug!(?addr, "ignoring a mutable item with a bad key or signature");
                    return;
                }
                if found.mutable.as_ref().is_none_or(|m| seq > m.seq) {
                    found.mutable = Some(MutableItem {
                        value,
                        seq,
                        signature: sig.0,
                        public_key: *public_key,
                    });
                }
            }
        }
    }
}

/// The mutable part of a BEP 44 `put` we send.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SignedMutable {
    public_key: [u8; 32],
    signature: [u8; 64],
    seq: i64,
    salt: Bytes,
    cas: Option<i64>,
}

/// A BEP 44 `put` we send.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PutArgs {
    token: ByteBufOwned,
    value: Bytes,
    mutable: Option<SignedMutable>,
}

/// Checks a value we are about to put: one canonically bencoded value, small enough for the
/// DHT. Other nodes refuse anything else.
fn check_item_value(value: &[u8]) -> crate::Result<()> {
    if value.len() > bep44::MAX_VALUE_SIZE {
        return Err(Error::InvalidItem("value over 1000 bytes"));
    }
    if !bep44::is_canonical_bencode(value) {
        return Err(Error::InvalidItem(
            "value is not one canonically bencoded value",
        ));
    }
    Ok(())
}

/// Checks the salt of a mutable item.
fn check_item_salt(salt: &[u8]) -> crate::Result<()> {
    if salt.len() > bep44::MAX_SALT_SIZE {
        return Err(Error::InvalidItem("salt over 64 bytes"));
    }
    Ok(())
}

impl RecursiveRequest<RecursiveRequestCallbacksGetPeers> {
    fn request_peers_forever(
        self: &Arc<Self>,
        mut node_rx: tokio::sync::mpsc::UnboundedReceiver<NodeToQuery>,
        is_v4: bool,
    ) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        spawn(
            debug_span!(parent: None, "get_peers", is_v4, info_hash = format!("{:?}", self.info_hash)),
            "get_peers",
            async move {
                let this = &this;
                // Looper adds root nodes to the queue every 60 seconds.
                let looper = {
                    async move {
                        let mut iteration = 0;
                        loop {
                            trace!("iteration {}", iteration);
                            let sleep = match this.get_peers_root(is_v4) {
                                Ok(0) => Duration::from_secs(1),
                                Ok(n) if n < 8 => REQUERY_INTERVAL / 8 * (n as u32),
                                Ok(_) => REQUERY_INTERVAL,
                                Err(e) => {
                                    error!("dht: error in get_peers_root(): {e:#}");
                                    return Err::<(), crate::Error>(e);
                                }
                            };
                            tokio::time::sleep(sleep).await;
                            iteration += 1;
                        }
                    }
                };
                tokio::pin!(looper);

                let mut futs = FuturesUnordered::new();
                loop {
                    tokio::select! {
                        node = node_rx.recv() => {
                            let node = node.unwrap();
                            let addr = node.addr;
                            futs.push(
                                this.request_one(node)
                                    .map_err(|e| debug!("error: {e:#}"))
                                    .instrument(debug_span!("addr", addr=addr.to_string()))
                            );
                        }
                        Some(_) = futs.next(), if !futs.is_empty() => {}
                        r = &mut looper => {
                            return r
                        }
                    }
                }
            },
        )
    }

    fn get_peers_root(&self, is_v4: bool) -> crate::Result<usize> {
        let mut count = 0;
        let table = if is_v4 {
            &self.dht.routing_table_v4
        } else {
            &self.dht.routing_table_v6
        };
        for (id, addr) in table
            .read()
            .sorted_by_distance_from(self.info_hash, now())
            .iter()
            .map(|n| (n.id(), n.addr()))
            .take(8)
        {
            count += 1;
            self.node_tx
                .send(NodeToQuery {
                    id: Some(id),
                    addr,
                    referrer: None,
                    depth: 0,
                })
                .ok()
                .ok_or(Error::DhtDead)?;
        }

        Ok(count)
    }
}

impl<C: RecursiveRequestCallbacks> RecursiveRequest<C> {
    async fn request_one(&self, node: NodeToQuery) -> crate::Result<()> {
        let NodeToQuery {
            id,
            addr,
            referrer,
            depth,
        } = node;
        if let Some(id) = id {
            self.callbacks.on_request_start(self, id, addr);
        }

        let response = self
            .dht
            .request_referred(self.request.clone(), addr, referrer)
            .await
            .inspect(|r| {
                self.mark_node_responded(addr, r);
            });
        if let Some(id) = id {
            self.callbacks.on_request_end(self, id, addr, &response);
        }

        let response = match response {
            Ok(ResponseOrError::Response(r)) => r,
            Ok(ResponseOrError::Error(e)) => {
                debug!("error response: {e:?}");
                return Err(Error::ErrorResponse);
            }
            Err(e) => {
                self.mark_node_error(addr);
                return Err(e);
            }
        };

        if let Some(peers) = response.values {
            for peer in peers {
                self.peer_tx.send(peer.0).ok().ok_or(Error::ReceiverDead)?;
            }
        }

        let node_it = response
            .nodes
            .iter()
            .flat_map(|n| n.iter().map(|n| n.as_socketaddr()))
            .chain(
                response
                    .nodes6
                    .iter()
                    .flat_map(|n| n.iter().map(|n| n.as_socketaddr())),
            )
            .filter(|node| addr.is_ipv4() == node.addr.is_ipv4())
            .take(MAX_NODES_PER_ANSWER);

        let now = now();
        // Other nodes list us too; asking ourselves is a wasted round trip (and a BEP 44 put
        // to ourselves).
        let ours = *self.dht.ids.read();

        for node in node_it.filter(|n| n.id != ours.v4 && n.id != ours.v6) {
            let should_request = self.should_request_node(node.id, node.addr, depth, now);
            trace!(
                "should_request={}, id={:?}, addr={}, depth={}/{}",
                should_request, node.id, node.addr, depth, self.max_depth
            );
            if should_request {
                self.node_tx
                    .send(NodeToQuery {
                        id: Some(node.id),
                        addr: node.addr,
                        referrer: Some(addr),
                        depth: depth + 1,
                    })
                    .ok()
                    .ok_or(Error::ReceiverDead)?;
            }
        }
        Ok(())
    }

    fn mark_node_error(&self, addr: SocketAddr) -> bool {
        self.useful_nodes
            .write()
            .iter_mut()
            .find(|n| n.addr == addr)
            .map(|n| {
                n.errors_in_a_row += 1;
            })
            .is_some()
    }

    fn mark_node_responded(&self, addr: SocketAddr, response: &ResponseOrError) -> bool {
        self.useful_nodes
            .write()
            .iter_mut()
            .find(|n| n.addr == addr)
            .map(|node| {
                node.last_response = Some(now());
                node.errors_in_a_row = 0;
                match response {
                    ResponseOrError::Response(r) => {
                        node.returned_peers =
                            r.values.as_ref().map(|c| !c.is_empty()).unwrap_or(false)
                    }
                    ResponseOrError::Error(_) => {
                        node.returned_peers = false;
                    }
                }
            })
            .is_some()
    }

    fn should_request_node(
        &self,
        node_id: Id20,
        addr: SocketAddr,
        depth: usize,
        now: Instant,
    ) -> bool {
        if depth >= self.max_depth {
            return false;
        }

        let mut closest_nodes = self.useful_nodes.write();

        // If recently requested, ignore
        if let Some(existing) = closest_nodes.iter_mut().find(|n| n.id == node_id) {
            if now - existing.last_request > Duration::from_secs(60) {
                existing.last_request = now;
                return true;
            }
            return false;
        }

        closest_nodes.push(MaybeUsefulNode {
            id: node_id,
            addr,
            last_request: now,
            last_response: None,
            returned_peers: false,
            errors_in_a_row: 0,
        });

        closest_nodes.sort_by_key(|n| {
            let has_returned_peers_desc = Reverse(n.returned_peers);
            let has_responded_desc = Reverse(n.last_response.is_some() as u8);
            let distance = n.id.distance(&self.info_hash);
            let freshest_response = n.last_response.map(|r| now - r).unwrap_or(Duration::MAX);
            (
                has_returned_peers_desc,
                has_responded_desc,
                distance,
                freshest_response,
            )
        });
        if closest_nodes.len() > self.useful_nodes_limit {
            let popped = closest_nodes.pop().unwrap();
            if popped.id == node_id {
                return false;
            }
        }
        true
    }
}

pub struct DhtState {
    // Lock order: a routing table, then `ids`. `ids` is only ever held to copy an id out or to
    // swap the ids, never across another lock.
    ids: RwLock<NodeIds>,
    next_request_serial: AtomicU64,

    // Created requests: (transaction_id, addr) => Requests.
    // If we get a response, it gets removed from here.
    inflight_by_transaction_id: DashMap<(u16, SocketAddr), OutstandingRequest>,

    routing_table_v4: RwLock<RoutingTable>,
    routing_table_v6: RwLock<RoutingTable>,
    listen_addr: SocketAddr,

    // BEP 42: our external address of each family, from the "ip" field of responses.
    ip_voter_v4: Mutex<ExternalIpVoter>,
    ip_voter_v6: Mutex<ExternalIpVoter>,
    enforce_node_id: AtomicBool,

    // Sending requests to the worker.
    rate_limiter: RateLimiter,
    // This is to send raw messages
    worker_sender: UnboundedSender<WorkerSendRequest>,

    cancellation_token: CancellationToken,

    pub(crate) peer_store: PeerStore,
    // BEP 44 items other nodes put to us.
    items: ItemStore,
    // The write tokens of BEP 44 `get` answers.
    write_tokens: WriteTokens,
}

impl DhtState {
    #[allow(clippy::too_many_arguments)]
    fn new_internal(
        id: Id20,
        sender: UnboundedSender<WorkerSendRequest>,
        routing_table_v4: Option<RoutingTable>,
        routing_table_v6: Option<RoutingTable>,
        listen_addr: SocketAddr,
        peer_store: PeerStore,
        enforce_node_id: bool,
        cancellation_token: CancellationToken,
    ) -> Self {
        // A restored table brings the id it was built around; with BEP 42 the ids of the two
        // address families differ.
        let routing_table_v4 = routing_table_v4.unwrap_or_else(|| RoutingTable::new(id, None));
        let routing_table_v6 = routing_table_v6.unwrap_or_else(|| RoutingTable::new(id, None));
        let ids = NodeIds {
            v4: routing_table_v4.id(),
            v6: routing_table_v6.id(),
        };
        peer_store.set_self_id(true, ids.v4);
        peer_store.set_self_id(false, ids.v6);
        Self {
            ids: RwLock::new(ids),
            next_request_serial: AtomicU64::new(0),
            inflight_by_transaction_id: Default::default(),
            routing_table_v4: RwLock::new(routing_table_v4),
            routing_table_v6: RwLock::new(routing_table_v6),
            worker_sender: sender,
            listen_addr,
            ip_voter_v4: Mutex::new(ExternalIpVoter::new()),
            ip_voter_v6: Mutex::new(ExternalIpVoter::new()),
            enforce_node_id: AtomicBool::new(enforce_node_id),
            rate_limiter: make_rate_limiter(),
            peer_store,
            items: ItemStore::default(),
            write_tokens: WriteTokens::new(now()),
            cancellation_token,
        }
    }

    /// Our node id towards nodes of the address family of `addr`.
    fn id_for(&self, addr: SocketAddr) -> Id20 {
        self.ids.read().get(addr.is_ipv4())
    }

    fn table(&self, is_v4: bool) -> &RwLock<RoutingTable> {
        if is_v4 {
            &self.routing_table_v4
        } else {
            &self.routing_table_v6
        }
    }

    /// Whether a node with `id` at `addr` may go into the routing table, be handed out to others
    /// and be a storage target: always, unless BEP 42 enforcement is on and the id does not
    /// match the address.
    fn node_id_acceptable(&self, id: &Id20, addr: SocketAddr) -> bool {
        !self.enforce_node_id.load(Ordering::Relaxed) || bep42::is_valid_node_id(id, addr.ip())
    }

    /// Whether a node that answered a lookup with a write token may store what we announce or
    /// put. With BEP 42 enforcement on, the id it answered with must match its address, and it
    /// must be the id the node was listed under: otherwise a node could be listed next to any
    /// target and answer with an id that is valid for its address but far from that target.
    fn storage_target_acceptable(
        &self,
        listed_id: Id20,
        answered_id: Id20,
        addr: SocketAddr,
    ) -> bool {
        if !self.enforce_node_id.load(Ordering::Relaxed) {
            return true;
        }
        listed_id == answered_id && bep42::is_valid_node_id(&answered_id, addr.ip())
    }

    /// Switches BEP 42 enforcement (see [`DhtConfig::enforce_node_id`]) on or off at runtime.
    /// Nodes already in the routing table stay, but are no longer handed out while it is on.
    pub fn set_enforce_node_id(&self, enforce: bool) {
        self.enforce_node_id.store(enforce, Ordering::Relaxed);
    }

    /// Pings the DHT node at `addr` and, when it answers, puts it into the routing table (like
    /// libtorrent's `add_dht_node`). For nodes known from elsewhere, such as the "nodes" of a
    /// torrent file (BEP 5): the bootstrap addresses only seed lookups and do not go into the
    /// routing table themselves. Returns the id the node answered with.
    pub async fn add_node(&self, addr: SocketAddr) -> crate::Result<Id20> {
        // Answers to an IPv4-mapped address come back from the plain IPv4 one, and the node
        // belongs in the IPv4 table.
        let addr = SocketAddr::new(addr.ip().to_canonical(), addr.port());
        let id = match self.request(Request::Ping, addr).await? {
            ResponseOrError::Response(r) => r.id,
            ResponseOrError::Error(e) => {
                debug!(?addr, error = ?e, "node answered a ping with an error");
                return Err(Error::ErrorResponse);
            }
        };
        if !self.node_id_acceptable(&id, addr) {
            debug!(?addr, ?id, "not adding a node with a non-BEP 42 id");
            return Ok(id);
        }
        let mut table = self.get_table_for_addr(addr).write();
        match table.add_node(id, addr) {
            InsertResult::WasExisting | InsertResult::ReplacedBad(_) | InsertResult::Added => {
                table.mark_response(&id, now());
            }
            InsertResult::Ignored => {}
        }
        Ok(id)
    }

    /// Counts the "ip" field of a response from `voter` towards our external address, and moves
    /// our node id when the vote adopts a new address. `referrer` is the node that pointed us to
    /// `voter`, if any: one referrer gets one vote per round, whatever number of nodes it
    /// listed.
    pub(crate) fn on_external_ip_reported(
        self: &Arc<Self>,
        voter: SocketAddr,
        referrer: Option<SocketAddr>,
        reported: SocketAddr,
    ) {
        let voter_ip = voter.ip().to_canonical();
        let is_v4 = voter_ip.is_ipv4();
        let voter_lock = if is_v4 {
            &self.ip_voter_v4
        } else {
            &self.ip_voter_v6
        };
        let adopted =
            voter_lock
                .lock()
                .vote(voter_ip, referrer.map(|r| r.ip()), reported.ip(), now());
        if let Some(ip) = adopted {
            self.on_external_ip_adopted(ip);
        }
    }

    /// Makes enough voters in distinct networks of the family of `ip` report `ip` as our
    /// address for the vote to adopt it, whether we have an address already or not.
    #[cfg(test)]
    pub(crate) fn vote_external_ip(self: &Arc<Self>, ip: IpAddr) {
        for n in 1..=bep42::VOTES_TO_REPLACE as u8 {
            let voter = match ip {
                IpAddr::V4(_) => SocketAddr::from(([n, 1, 1, 1], 6881)),
                IpAddr::V6(_) => SocketAddr::from((
                    Ipv6Addr::new(0x2001, 0x0db8, u16::from(n), 0, 0, 0, 0, 1),
                    6881,
                )),
            };
            self.on_external_ip_reported(voter, None, SocketAddr::new(ip, 4321));
        }
    }

    fn on_external_ip_adopted(self: &Arc<Self>, ip: IpAddr) {
        let is_v4 = ip.is_ipv4();
        let current = self.ids.read().get(is_v4);
        // An id that matches already (e.g. one restored from disk) is kept, so that restarts
        // do not move us around the DHT.
        if bep42::node_id_matches_ip(&current, ip) {
            info!(%ip, id = ?current, "adopted external address, node id already matches it");
            return;
        }
        let id = bep42::generate_node_id(ip);
        info!(%ip, old_id = ?current, new_id = ?id, "adopted external address, new node id");
        self.set_node_id(is_v4, id);
        self.refresh_around(is_v4, id);
    }

    /// Moves us to `id` in the DHT of one address family: the routing table is rebuilt around
    /// the new id with the nodes we know, and everything that uses our id follows.
    fn set_node_id(&self, is_v4: bool, id: Id20) {
        let mut table = self.table(is_v4).write();
        *table = table.with_new_id(id);
        let mut ids = self.ids.write();
        if is_v4 {
            ids.v4 = id;
        } else {
            ids.v6 = id;
        }
        self.peer_store.set_self_id(is_v4, id);
    }

    /// Looks up the nodes around `id`, so that the buckets near our new id fill up.
    fn refresh_around(self: &Arc<Self>, is_v4: bool, id: Id20) {
        let addrs = self
            .table(is_v4)
            .read()
            .sorted_by_distance_from(id, now())
            .iter()
            .map(|n| n.addr())
            .take(8)
            .collect::<Vec<_>>();
        if addrs.is_empty() {
            return;
        }
        let dht = self.clone();
        spawn_with_cancel(
            debug_span!(parent: None, "dht_refresh_new_id", is_v4),
            "dht_refresh_new_id",
            self.cancellation_token.clone(),
            async move {
                if let Err(e) =
                    RecursiveRequest::find_node_for_routing_table(dht, id, addrs.into_iter()).await
                {
                    debug!("lookup around the new node id failed: {e:#}");
                }
                Ok::<_, Error>(())
            },
        );
    }

    async fn request(&self, request: Request, addr: SocketAddr) -> crate::Result<ResponseOrError> {
        self.request_referred(request, addr, None).await
    }

    /// Sends `request` to `addr` and waits for the answer. `referrer` is the node that pointed
    /// us to `addr`, if any (see `NodeToQuery::referrer`).
    async fn request_referred(
        &self,
        request: Request,
        addr: SocketAddr,
        referrer: Option<SocketAddr>,
    ) -> crate::Result<ResponseOrError> {
        self.rate_limiter.acquire_one().await;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let serial = self.next_request_serial.fetch_add(1, Ordering::Relaxed);
        let tid = loop {
            let tid = random_transaction_id();
            if let dashmap::Entry::Vacant(e) = self.inflight_by_transaction_id.entry((tid, addr)) {
                e.insert(OutstandingRequest {
                    done: tx,
                    referrer,
                    serial,
                });
                break tid;
            }
        };
        let _guard = InflightGuard {
            map: &self.inflight_by_transaction_id,
            key: (tid, addr),
            serial,
        };
        let message = self.create_request(tid, request, addr);
        trace!("sending {message:?}");
        if self
            .worker_sender
            .send(WorkerSendRequest {
                our_tid: Some(tid),
                message,
                addr,
            })
            .is_err()
        {
            return Err(Error::DhtDead);
        }
        match tokio::time::timeout(RESPONSE_TIMEOUT, rx).await {
            Ok(Ok(r)) => r.map(|r| {
                trace!("received {r:?}");
                r
            }),
            Ok(Err(_)) => Err(Error::DhtDead),
            Err(_) => Err(Error::ResponseTimeout(RESPONSE_TIMEOUT)),
        }
    }

    fn create_request(
        &self,
        transaction_id: u16,
        request: Request,
        addr: SocketAddr,
    ) -> Message<ByteBufOwned> {
        let transaction_id_buf = transaction_id.to_be_bytes();

        let want = if addr.is_ipv6() {
            Some(Want::V6)
        } else {
            Some(Want::V4)
        };
        let id = self.id_for(addr);

        match request {
            Request::GetPeers(info_hash) => Message {
                transaction_id: ByteBufOwned::from(transaction_id_buf.as_ref()),
                version: None,
                ip: None,
                kind: MessageKind::GetPeersRequest(GetPeersRequest {
                    id,
                    info_hash,
                    want,
                }),
            },
            Request::FindNode(target) => Message {
                transaction_id: ByteBufOwned::from(transaction_id_buf.as_ref()),
                version: None,
                ip: None,
                kind: MessageKind::FindNodeRequest(FindNodeRequest { id, target, want }),
            },
            Request::Ping => Message {
                transaction_id: ByteBufOwned::from(transaction_id_buf.as_ref()),
                version: None,
                ip: None,
                kind: MessageKind::PingRequest(PingRequest { id }),
            },
            Request::Announce {
                info_hash,
                token,
                port,
            } => Message {
                kind: MessageKind::AnnouncePeer(AnnouncePeer {
                    id,
                    implied_port: 0,
                    info_hash,
                    port,
                    token,
                }),
                transaction_id: ByteBufOwned::from(transaction_id_buf.as_ref()),
                version: None,
                ip: None,
            },
            Request::Get(target) => Message {
                transaction_id: ByteBufOwned::from(transaction_id_buf.as_ref()),
                version: None,
                ip: None,
                kind: MessageKind::GetRequest(GetRequest {
                    id,
                    target,
                    seq: None,
                    want,
                }),
            },
            Request::Put(args) => {
                let PutArgs {
                    token,
                    value,
                    mutable,
                } = *args;
                let mut put = PutRequest {
                    id,
                    token,
                    v: RawBencode(ByteBufOwned::from(value)),
                    k: None,
                    sig: None,
                    seq: None,
                    salt: None,
                    cas: None,
                };
                if let Some(m) = mutable {
                    put.k = Some(FixedBytes(m.public_key));
                    put.sig = Some(FixedBytes(m.signature));
                    put.seq = Some(m.seq);
                    put.salt = (!m.salt.is_empty()).then(|| ByteBufOwned::from(m.salt));
                    put.cas = m.cas;
                }
                Message {
                    transaction_id: ByteBufOwned::from(transaction_id_buf.as_ref()),
                    version: None,
                    ip: None,
                    kind: MessageKind::PutRequest(put),
                }
            }
        }
    }

    fn generate_compact_nodes_both(
        &self,
        target: Id20,
        want: Want,
    ) -> (
        Option<CompactNodeInfoOwned<SocketAddrV4>>,
        Option<CompactNodeInfoOwned<SocketAddrV6>>,
    ) {
        let now = now();
        match want {
            Want::V4 => (
                Some(self.generate_compact_nodes(target, &self.routing_table_v4.read(), now)),
                None,
            ),
            Want::V6 => (
                None,
                Some(self.generate_compact_nodes(target, &self.routing_table_v6.read(), now)),
            ),
            Want::Both => (
                Some(self.generate_compact_nodes(target, &self.routing_table_v4.read(), now)),
                Some(self.generate_compact_nodes(target, &self.routing_table_v6.read(), now)),
            ),
            Want::None => (None, None),
        }
    }

    fn get_table_for_addr(&self, addr: SocketAddr) -> &RwLock<RoutingTable> {
        if addr.is_ipv4() {
            &self.routing_table_v4
        } else {
            &self.routing_table_v6
        }
    }

    fn generate_compact_nodes<A>(
        &self,
        target: Id20,
        table: &RoutingTable,
        now: Instant,
    ) -> CompactNodeInfo<ByteBufOwned, A>
    where
        A: CompactSerialize + CompactSerializeFixedLen + FromSocketAddr,
        Node<A>: CompactSerialize + CompactSerializeFixedLen,
    {
        let enforce = self.enforce_node_id.load(Ordering::Relaxed);
        let it = table
            .sorted_by_distance_from(target, now)
            .into_iter()
            .filter(|r| !enforce || bep42::is_valid_node_id(&r.id(), r.addr().ip()))
            .filter_map(|r| {
                Some(Node {
                    id: r.id(),
                    addr: A::from_socket_addr(r.addr())?,
                })
            })
            .take(8);
        CompactNodeInfo::new_from_iter(it)
    }

    fn on_received_message(
        self: &Arc<Self>,
        msg: Message<ByteBufOwned>,
        addr: SocketAddr,
    ) -> crate::Result<()> {
        match &msg.kind {
            // If it's a response to a request we made, find the request task, notify it with the response,
            // and let it handle it.
            MessageKind::Error(_) | MessageKind::Response(_) => {
                let tid = msg
                    .get_our_transaction_id()
                    .ok_or(Error::BadTransactionId)?;
                let request = match self
                    .inflight_by_transaction_id
                    .remove(&(tid, addr))
                    .map(|(_, v)| v)
                {
                    Some(req) => req,
                    None => {
                        trace!(?msg, "outstanding request not found");
                        return Err(Error::RequestNotFound);
                    }
                };

                // BEP 42: whoever answers our queries tells us how they see our address.
                // Errors count as well: a node that enforces BEP 42 answers a node with an id
                // that does not match its address with an error.
                if let Some(reported) = msg.ip {
                    self.on_external_ip_reported(addr, request.referrer, reported);
                }

                let response_or_error = match msg.kind {
                    MessageKind::Error(e) => ResponseOrError::Error(e),
                    MessageKind::Response(r) => ResponseOrError::Response(Box::new(r)),
                    _ => unreachable!(),
                };
                match request.done.send(Ok(response_or_error)) {
                    Ok(_) => {}
                    Err(e) => {
                        debug!(
                            "received response, but the receiver task is closed: {:?}",
                            e
                        );
                    }
                }
                return Ok(());
            }
            _ => {}
        };

        trace!("received query from {addr}: {msg:?}");

        match &msg.kind {
            // Otherwise, respond to a query.
            MessageKind::PingRequest(req) => {
                let message = Message {
                    transaction_id: msg.transaction_id,
                    version: None,
                    ip: Some(addr),
                    kind: MessageKind::Response(bprotocol::Response {
                        id: self.id_for(addr),
                        ..Default::default()
                    }),
                };
                self.get_table_for_addr(addr)
                    .write()
                    .mark_last_query(&req.id, now());
                self.worker_sender
                    .send(WorkerSendRequest {
                        our_tid: None,
                        message,
                        addr,
                    })
                    .ok()
                    .ok_or(Error::DhtDead)?;
                Ok(())
            }
            MessageKind::AnnouncePeer(ann) => {
                self.get_table_for_addr(addr)
                    .write()
                    .mark_last_query(&ann.id, now());
                let added = self.peer_store.store_peer(ann, addr);
                trace!("{addr}: added_peer={added}, announce={ann:?}");
                let message = Message {
                    transaction_id: msg.transaction_id,
                    version: None,
                    ip: Some(addr),
                    kind: MessageKind::Response(bprotocol::Response {
                        id: self.id_for(addr),
                        ..Default::default()
                    }),
                };
                self.worker_sender
                    .send(WorkerSendRequest {
                        our_tid: None,
                        message,
                        addr,
                    })
                    .ok()
                    .ok_or(Error::DhtDead)?;
                Ok(())
            }
            MessageKind::GetPeersRequest(req) => {
                let want = req
                    .want
                    .unwrap_or(if addr.is_ipv6() { Want::V6 } else { Want::V4 });
                let (nodes, nodes6) = self.generate_compact_nodes_both(req.info_hash, want);
                let compact_peer_info = self.peer_store.get_for_info_hash(req.info_hash, want);
                self.get_table_for_addr(addr)
                    .write()
                    .mark_last_query(&req.id, now());
                let message = Message {
                    transaction_id: msg.transaction_id,
                    version: None,
                    ip: Some(addr),
                    kind: MessageKind::Response(bprotocol::Response {
                        id: self.id_for(addr),
                        nodes,
                        nodes6,
                        values: Some(compact_peer_info),
                        token: Some(ByteBufOwned::from(
                            &self.peer_store.gen_token_for(req.id, addr)[..],
                        )),
                        ..Default::default()
                    }),
                };
                self.worker_sender
                    .send(WorkerSendRequest {
                        our_tid: None,
                        message,
                        addr,
                    })
                    .ok()
                    .ok_or(Error::DhtDead)?;
                Ok(())
            }
            MessageKind::FindNodeRequest(req) => {
                let want = req
                    .want
                    .unwrap_or(if addr.is_ipv6() { Want::V6 } else { Want::V4 });
                let (nodes, nodes6) = self.generate_compact_nodes_both(req.target, want);
                self.get_table_for_addr(addr)
                    .write()
                    .mark_last_query(&req.id, now());
                let message = Message {
                    transaction_id: msg.transaction_id,
                    version: None,
                    ip: Some(addr),
                    kind: MessageKind::Response(bprotocol::Response {
                        id: self.id_for(addr),
                        nodes,
                        nodes6,
                        ..Default::default()
                    }),
                };
                self.worker_sender
                    .send(WorkerSendRequest {
                        our_tid: None,
                        message,
                        addr,
                    })
                    .ok()
                    .ok_or(Error::DhtDead)?;
                Ok(())
            }
            MessageKind::GetRequest(req) => {
                let want = req
                    .want
                    .unwrap_or(if addr.is_ipv6() { Want::V6 } else { Want::V4 });
                let (nodes, nodes6) = self.generate_compact_nodes_both(req.target, want);
                self.get_table_for_addr(addr)
                    .write()
                    .mark_last_query(&req.id, now());
                let mut response = bprotocol::Response {
                    id: self.id_for(addr),
                    nodes,
                    nodes6,
                    token: Some(ByteBufOwned::from(
                        &self.write_tokens.token(addr.ip(), &req.target, now())[..],
                    )),
                    ..Default::default()
                };
                match self.items.get(&req.target, now()) {
                    Some(StoredView::Immutable(value)) => {
                        response.v = Some(RawBencode(ByteBufOwned::from(value)));
                    }
                    Some(StoredView::Mutable(item)) => {
                        response.seq = Some(item.seq);
                        // The requester already has everything up to its "seq": then only our
                        // "seq" goes back, as BEP 44 asks.
                        if req.seq.is_none_or(|seq| item.seq > seq) {
                            response.k = Some(FixedBytes(item.public_key));
                            response.sig = Some(FixedBytes(item.signature));
                            response.v = Some(RawBencode(ByteBufOwned::from(item.value)));
                        }
                    }
                    None => {}
                }
                self.reply(
                    addr,
                    msg.transaction_id.clone(),
                    MessageKind::Response(response),
                )
            }
            MessageKind::PutRequest(req) => {
                self.get_table_for_addr(addr)
                    .write()
                    .mark_last_query(&req.id, now());
                let kind = match self.store_item(req, addr) {
                    Ok(()) => MessageKind::Response(bprotocol::Response {
                        id: self.id_for(addr),
                        ..Default::default()
                    }),
                    Err(e) => {
                        debug!(?addr, code = e.code(), "refusing put: {}", e.message());
                        MessageKind::Error(ErrorDescription {
                            code: e.code(),
                            description: ByteBufOwned::from(e.message().as_bytes()),
                        })
                    }
                };
                self.reply(addr, msg.transaction_id.clone(), kind)
            }
            _ => unreachable!(),
        }
    }

    /// Answers the query with `transaction_id` from `addr`.
    fn reply(
        &self,
        addr: SocketAddr,
        transaction_id: ByteBufOwned,
        kind: MessageKind<ByteBufOwned>,
    ) -> crate::Result<()> {
        let message = Message {
            transaction_id,
            version: None,
            ip: Some(addr),
            kind,
        };
        self.worker_sender
            .send(WorkerSendRequest {
                our_tid: None,
                message,
                addr,
            })
            .ok()
            .ok_or(Error::DhtDead)
    }

    /// Checks a BEP 44 `put` from `addr` and stores the item.
    fn store_item(
        &self,
        req: &PutRequest<ByteBufOwned>,
        addr: SocketAddr,
    ) -> Result<(), bep44::PutError> {
        let now = now();
        let item = bep44::check_put(req, |target| {
            self.write_tokens
                .is_valid(req.token.as_ref(), addr.ip(), target, now)
        })?;
        let ids = *self.ids.read();
        self.items.put(item, addr.ip(), [ids.v4, ids.v6], now)
    }

    /// Our known nodes closest to `target`, from both routing tables: where a lookup starts.
    fn closest_known_nodes(&self, target: Id20) -> Vec<(Option<Id20>, SocketAddr)> {
        let now = now();
        [&self.routing_table_v4, &self.routing_table_v6]
            .into_iter()
            .flat_map(|table| {
                table
                    .read()
                    .sorted_by_distance_from(target, now)
                    .iter()
                    .take(PUT_TARGETS)
                    .map(|n| (Some(n.id()), n.addr()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Runs a BEP 44 `get` lookup for `target` through the DHT, both address families at once,
    /// and returns the item found and the closest nodes that gave us a write token. With
    /// `stop_at_immutable`, it ends as soon as a checked immutable value arrives.
    async fn item_lookup(
        self: &Arc<Self>,
        target: Id20,
        query: ItemQuery,
        stop_at_immutable: bool,
    ) -> crate::Result<ItemLookupFound> {
        let seeds = self.closest_known_nodes(target);
        if seeds.is_empty() {
            return Err(Error::NoSuccessfulLookups { errors: 0 });
        }
        let (node_tx, node_rx) = unbounded_channel();
        let req = RecursiveRequest {
            max_depth: 4,
            info_hash: target,
            request: Request::Get(target),
            dht: self.clone(),
            useful_nodes_limit: 32,
            useful_nodes: RwLock::new(Vec::new()),
            peer_tx: unbounded_channel().0,
            node_tx,
            callbacks: RecursiveRequestCallbacksGetItem {
                query,
                found: Mutex::new(ItemLookupFound {
                    // What we store ourselves counts: we are one of the nodes closest to it.
                    // It was checked when it was put to us.
                    mutable: match self.items.get(&target, now()) {
                        Some(StoredView::Mutable(item)) => Some(item),
                        _ => None,
                    },
                    ..Default::default()
                }),
                immutable_found: tokio::sync::Notify::new(),
            },
        };
        let run = req.run_to_completion(node_rx, seeds.into_iter());
        let found_it = async {
            if stop_at_immutable {
                req.callbacks.immutable_found.notified().await
            } else {
                std::future::pending().await
            }
        };
        let lookup = async {
            tokio::select! {
                r = run => r,
                () = found_it => Ok(()),
            }
        };
        match tokio::time::timeout(ITEM_LOOKUP_DEADLINE, lookup)
            .instrument(debug_span!("dht_get", target = ?target))
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => debug!(?target, "lookup deadline reached, using what was found"),
        }
        Ok(std::mem::take(&mut *req.callbacks.found.lock()))
    }

    /// Puts an item to `targets`, and returns how many of them stored it. Errors when none
    /// did, with what the nodes answered.
    async fn put_item(
        &self,
        targets: Vec<PutTarget>,
        value: Bytes,
        mutable: Option<SignedMutable>,
    ) -> crate::Result<usize> {
        let asked = targets.len();
        let answers = futures::future::join_all(targets.into_iter().map(|t| {
            let mut mutable = mutable.clone();
            // BEP 44: `cas` is not sent to a node that had no version of the item, which could
            // only refuse it.
            if let Some(m) = &mut mutable
                && !t.has_item
            {
                m.cas = None;
            }
            let args = PutArgs {
                token: t.token,
                value: value.clone(),
                mutable,
            };
            self.request(Request::Put(Box::new(args)), t.addr)
        }))
        .await;
        let mut stored = 0;
        let mut rejections = Vec::new();
        for answer in answers {
            match answer {
                Ok(ResponseOrError::Response(_)) => stored += 1,
                Ok(ResponseOrError::Error(e)) => rejections.push((
                    e.code,
                    String::from_utf8_lossy(e.description.as_ref()).into_owned(),
                )),
                Err(e) => debug!("put failed: {e:#}"),
            }
        }
        if stored == 0 {
            return Err(Error::ItemNotStored { asked, rejections });
        }
        Ok(stored)
    }

    /// Looks up the immutable BEP 44 item `target` (the SHA-1 of its bencoded value) and returns
    /// its value, bencoded, byte for byte as stored. Values that do not hash to `target` are
    /// ignored. `None` when no node has it.
    pub async fn get_immutable(self: &Arc<Self>, target: Id20) -> crate::Result<Option<Bytes>> {
        // An immutable item is the same everywhere, so if it was put to us we are done.
        if let Some(StoredView::Immutable(value)) = self.items.get(&target, now()) {
            return Ok(Some(value));
        }
        Ok(self
            .item_lookup(target, ItemQuery::Immutable, true)
            .await?
            .immutable)
    }

    /// Stores `value` (one bencoded value of at most [`bep44::MAX_VALUE_SIZE`] bytes) as an
    /// immutable BEP 44 item on the nodes closest to it, and returns its target. Errors when no
    /// node stored it.
    pub async fn put_immutable(self: &Arc<Self>, value: Bytes) -> crate::Result<Id20> {
        check_item_value(&value)?;
        let target = bep44::immutable_target(&value);
        let found = self
            .item_lookup(target, ItemQuery::Immutable, false)
            .await?;
        self.put_item(found.put_targets, value, None).await?;
        Ok(target)
    }

    /// Looks up the mutable BEP 44 item of `public_key` and `salt` (empty for none) and returns
    /// the version with the greatest sequence number among those with a valid signature. `None`
    /// when no node has it. A version put to us counts too, and is returned when the lookup
    /// reaches no node at all.
    pub async fn get_mutable(
        self: &Arc<Self>,
        public_key: [u8; 32],
        salt: &[u8],
    ) -> crate::Result<Option<MutableItem>> {
        check_item_salt(salt)?;
        let target = bep44::mutable_target(&public_key, salt);
        let query = ItemQuery::Mutable {
            public_key,
            salt: Bytes::copy_from_slice(salt),
        };
        match self.item_lookup(target, query, false).await {
            Ok(found) => Ok(found.mutable),
            Err(e) => match self.items.get(&target, now()) {
                Some(StoredView::Mutable(item)) => Ok(Some(item)),
                _ => Err(e),
            },
        }
    }

    /// Signs `value` (one bencoded value of at most [`bep44::MAX_VALUE_SIZE`] bytes) with the
    /// ed25519 key whose 32-byte secret is `signing_key`, and stores it as version `seq` of the
    /// mutable BEP 44 item of that key and `salt` (empty for none, at most
    /// [`bep44::MAX_SALT_SIZE`] bytes) on the nodes closest to it. With `cas`, nodes that have
    /// a version only take it if theirs is `cas`. Errors when no node stored it; nodes that
    /// refuse it (e.g. because they have a newer version, code 302, or `cas` did not match, code
    /// 301) are listed in the error.
    pub async fn put_mutable(
        self: &Arc<Self>,
        signing_key: &[u8; 32],
        salt: &[u8],
        seq: i64,
        value: Bytes,
        cas: Option<i64>,
    ) -> crate::Result<()> {
        check_item_value(&value)?;
        check_item_salt(salt)?;
        if seq < 0 {
            return Err(Error::InvalidItem("negative sequence number"));
        }
        let item = MutableItem {
            signature: bep44::sign_mutable(signing_key, salt, seq, &value),
            public_key: bep44::public_key(signing_key),
            value,
            seq,
        };
        self.put_mutable_signed(item, salt, cas).await
    }

    /// Stores a mutable BEP 44 item that is already signed, e.g. one found with
    /// [`Self::get_mutable`], under `salt` (the salt it was signed with) on the nodes closest
    /// to it: anyone can keep an item alive without the private key. `cas` and the errors are
    /// as for [`Self::put_mutable`]; an item whose signature does not verify is refused before
    /// anything is sent.
    pub async fn put_mutable_signed(
        self: &Arc<Self>,
        item: MutableItem,
        salt: &[u8],
        cas: Option<i64>,
    ) -> crate::Result<()> {
        check_item_value(&item.value)?;
        check_item_salt(salt)?;
        if item.seq < 0 {
            return Err(Error::InvalidItem("negative sequence number"));
        }
        if !bep44::verify_mutable(
            &item.public_key,
            salt,
            item.seq,
            &item.value,
            &item.signature,
        ) {
            return Err(Error::InvalidItem("signature does not verify"));
        }
        let salt = Bytes::copy_from_slice(salt);
        let target = bep44::mutable_target(&item.public_key, &salt);
        let query = ItemQuery::Mutable {
            public_key: item.public_key,
            salt: salt.clone(),
        };
        let found = self.item_lookup(target, query, false).await?;
        let mutable = SignedMutable {
            public_key: item.public_key,
            signature: item.signature,
            seq: item.seq,
            salt,
            cas,
        };
        self.put_item(found.put_targets, item.value, Some(mutable))
            .await?;
        Ok(())
    }

    pub fn get_stats(&self) -> DhtStats {
        let ids = *self.ids.read();
        DhtStats {
            id: ids.v4,
            id_v6: ids.v6,
            outstanding_requests: self.inflight_by_transaction_id.len(),
            routing_table_size: self.routing_table_v4.read().len(),
            routing_table_size_v6: self.routing_table_v6.read().len(),
            external_ip_v4: bep42::adopted_v4(&self.ip_voter_v4.lock()),
            external_ip_v6: bep42::adopted_v6(&self.ip_voter_v6.lock()),
            stored_items: self.items.len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Request {
    GetPeers(Id20),
    FindNode(Id20),
    Announce {
        info_hash: Id20,
        token: ByteBufOwned,
        port: u16,
    },
    Ping,
    /// BEP 44 `get` of the item with this target.
    Get(Id20),
    /// BEP 44 `put`. Boxed: it is far bigger than the other requests.
    Put(Box<PutArgs>),
}

enum ResponseOrError {
    // Boxed: with the BEP 44 fields a response is several times bigger than an error.
    Response(Box<Response<ByteBufOwned>>),
    Error(ErrorDescription<ByteBufOwned>),
}

impl core::fmt::Debug for ResponseOrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Response(r) => write!(f, "{r:?}"),
            Self::Error(e) => write!(f, "{e:?}"),
        }
    }
}

struct DhtWorker {
    socket: UdpSocket,
    dht: Arc<DhtState>,
}

// Windows fails recv_from on a UDP socket with these for a single datagram, and the socket keeps
// working afterwards:
// - WSAEMSGSIZE: the datagram was longer than the buffer and got truncated.
// - WSAENETRESET: an earlier send_to got an ICMP time exceeded back.
// - WSAECONNRESET: an earlier send_to got an ICMP port unreachable back.
fn is_datagram_error(e: &std::io::Error) -> bool {
    const WSAEMSGSIZE: i32 = 10040;
    const WSAENETRESET: i32 = 10052;
    const WSAECONNRESET: i32 = 10054;
    cfg!(windows)
        && matches!(
            e.raw_os_error(),
            Some(WSAEMSGSIZE | WSAENETRESET | WSAECONNRESET)
        )
}

impl DhtWorker {
    fn on_send_error(&self, tid: u16, addr: SocketAddr, err: crate::Error) {
        if let Some((_, OutstandingRequest { done, .. })) =
            self.dht.inflight_by_transaction_id.remove(&(tid, addr))
        {
            let _ = done.send(Err(err)).is_err();
        };
    }

    async fn bootstrap_hostname(&self, hostname: &str) -> crate::Result<()> {
        let addrs = tokio::net::lookup_host(hostname)
            .await
            .map_err(|err| Error::lookup(hostname, err))?
            .collect::<Vec<_>>();
        let ids = *self.dht.ids.read();
        let v4 = RecursiveRequest::find_node_for_routing_table(
            self.dht.clone(),
            ids.v4,
            addrs.iter().copied().filter(|a| a.is_ipv4()),
        )
        .instrument(debug_span!("v4"));

        let v6 = RecursiveRequest::find_node_for_routing_table(
            self.dht.clone(),
            ids.v6,
            addrs.iter().copied().filter(|a| a.is_ipv6()),
        )
        .instrument(debug_span!("v6"));

        let (v4, v6) = tokio::join!(v4, v6);
        v4.or(v6)
    }

    async fn bootstrap_hostname_with_backoff(&self, addr: &str) -> crate::Result<()> {
        let backoff = ExponentialBuilder::new()
            .with_max_delay(Duration::from_secs(60))
            .with_jitter()
            .with_total_delay(Some(Duration::from_secs(86400)))
            .without_max_times();

        (|| self.bootstrap_hostname(addr))
            .retry(backoff)
            .notify(|error, retry_in| {
                warn!(?retry_in, ?addr, "error in bootstrap: {error:#}");
            })
            .instrument(debug_span!("bootstrap", hostname = addr))
            .await
    }

    async fn bootstrap(&self, bootstrap_addrs: &[String]) -> crate::Result<()> {
        let mut futs = FuturesUnordered::new();

        for addr in bootstrap_addrs.iter() {
            futs.push(self.bootstrap_hostname_with_backoff(addr));
        }
        let mut successes = 0;
        while let Some(resp) = futs.next().await {
            if resp.is_ok() {
                successes += 1
            }
        }
        if successes == 0 {
            return Err(Error::BootstrapFailed);
        }
        Ok(())
    }

    async fn bucket_refresher(&self, is_v4: bool) -> crate::Result<()> {
        let (tx, mut rx) = unbounded_channel();

        let table = if is_v4 {
            &self.dht.routing_table_v4
        } else {
            &self.dht.routing_table_v6
        };

        let mut futs = FuturesUnordered::new();
        let filler = async {
            let mut interval = tokio::time::interval(INACTIVITY_TIMEOUT);
            interval.tick().await;
            let mut iteration = 0;
            loop {
                interval.tick().await;
                let now = now();
                let mut found = 0;

                for bucket in table.read().iter_buckets() {
                    if now - bucket.leaf.last_refreshed < INACTIVITY_TIMEOUT {
                        continue;
                    }
                    found += 1;
                    let random_id = bucket.random_within();
                    tx.send(random_id).unwrap();
                }
                trace!("iteration {}, refreshing {} buckets", iteration, found);
                iteration += 1;
            }
        };

        tokio::pin!(filler);

        loop {
            tokio::select! {
                _ = &mut filler => {},
                random_id = rx.recv() => {
                    let random_id = random_id.unwrap();
                    let addrs = table
                        .read()
                        .sorted_by_distance_from(random_id, now())
                        .iter()
                        .map(|n| n.addr())
                        .take(8).collect::<Vec<_>>();
                    futs.push(
                        RecursiveRequest::find_node_for_routing_table(
                            self.dht.clone(), random_id, addrs.into_iter()
                        ).instrument(debug_span!("refresh_bucket"))
                    );
                },
                _ = futs.next(), if !futs.is_empty() => {},
            }
        }
    }

    async fn pinger(&self, is_v4: bool) -> crate::Result<()> {
        let table = if is_v4 {
            &self.dht.routing_table_v4
        } else {
            &self.dht.routing_table_v6
        };
        let mut futs = FuturesUnordered::new();
        let mut interval = tokio::time::interval(INACTIVITY_TIMEOUT / 4);
        let (tx, mut rx) = unbounded_channel();
        let looper = async {
            let mut iteration = 0;
            loop {
                interval.tick().await;
                let mut found = 0;
                let now = now();
                for node in table.read().iter() {
                    if matches!(
                        node.status(now),
                        NodeStatus::Questionable | NodeStatus::Unknown
                    ) {
                        found += 1;
                        tx.send((node.id(), node.addr())).unwrap();
                    }
                }
                trace!("iteration {}, pinging {} nodes", iteration, found);
                iteration += 1;
            }
        };

        tokio::pin!(looper);

        loop {
            tokio::select! {
                _ = &mut looper => {},
                r = rx.recv() => {
                    let (id, addr) = r.unwrap();
                    futs.push(async move {
                        table.write().mark_outgoing_request(&id, now());
                        match self.dht.request(Request::Ping, addr).await {
                            Ok(_) => {
                                table.write().mark_response(&id, now());
                            },
                            Err(e) => {
                                table.write().mark_error(&id);
                                debug!("error: {e:#}");
                            }
                        }
                    }.instrument(debug_span!("ping", addr=addr.to_string())))
                },
                _ = futs.next(), if !futs.is_empty() => {},
            }
        }
    }

    async fn framer(
        &self,
        socket: &UdpSocket,
        mut input_rx: UnboundedReceiver<WorkerSendRequest>,
        output_tx: Sender<(Message<ByteBufOwned>, SocketAddr)>,
    ) -> crate::Result<()> {
        let writer = async {
            let mut buf = Vec::new();
            while let Some(WorkerSendRequest {
                our_tid,
                message,
                addr,
            }) = input_rx.recv().await
            {
                if our_tid.is_none() {
                    trace!("{}: sending {:?}", addr, &message);
                }
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
                    debug!("error sending to {addr}: {e:#}");
                    if let Some(tid) = our_tid {
                        self.on_send_error(tid, addr, Error::Send(e));
                    }
                }
            }
            Err(Error::DhtDead)
        };
        let reader = async {
            let mut buf = vec![0u8; 16384];
            loop {
                let (size, addr) = match socket.recv_from(&mut buf).await {
                    Ok(r) => r,
                    Err(e) if is_datagram_error(&e) => {
                        debug!("ignoring UDP receive error: {e:#}");
                        continue;
                    }
                    Err(e) => return Err(Error::Recv(e)),
                };
                match bprotocol::deserialize_message::<ByteBufOwned>(&buf[..size]) {
                    Ok(msg) => match output_tx.send((msg, addr)).await {
                        Ok(_) => {}
                        Err(_) => return Err(Error::DhtDead),
                    },
                    Err(e) => {
                        debug!("{}: error deserializing incoming message: {}", addr, e);
                        if let Some(tid) = bprotocol::malformed_put_transaction_id(&buf[..size]) {
                            let error = ErrorDescription {
                                code: 203,
                                description: ByteBufOwned::from(&b"malformed put"[..]),
                            };
                            self.dht.reply(addr, tid, MessageKind::Error(error))?;
                        }
                    }
                }
            }
        };
        let result = tokio::select! {
            err = writer => err,
            err = reader => err,
        };
        result
    }

    async fn start(
        self,
        in_rx: UnboundedReceiver<WorkerSendRequest>,
        bootstrap_addrs: &[String],
    ) -> crate::Result<()> {
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
                    if let Err(e) = this.dht.on_received_message(response, addr) {
                        debug!("error in on_response, addr={:?}: {}", addr, e)
                    }
                }
                Err(Error::DhtDead)
            }
        }
        .instrument(debug_span!("dht_response_reader"));

        let pinger_v4 = self.pinger(true).instrument(debug_span!("pinger_v4"));
        let bucket_refresher_v4 = self
            .bucket_refresher(true)
            .instrument(debug_span!("bucket_refresher_v4"));

        let pinger_v6 = self.pinger(false).instrument(debug_span!("pinger_v6"));
        let bucket_refresher_v6 = self
            .bucket_refresher(false)
            .instrument(debug_span!("bucket_refresher_v6"));

        tokio::pin!(framer);
        tokio::pin!(bootstrap);
        tokio::pin!(response_reader);
        tokio::pin!(pinger_v4);
        tokio::pin!(bucket_refresher_v4);
        tokio::pin!(pinger_v6);
        tokio::pin!(bucket_refresher_v6);

        loop {
            tokio::select! {
                err = &mut framer => {
                    return Error::task_finished(&"framer", err);
                },
                result = &mut bootstrap, if !bootstrap_done => {
                    bootstrap_done = true;
                    result?;
                },
                err = &mut pinger_v4 => {
                    return Error::task_finished(&"pinger_v4", err);
                },
                err = &mut bucket_refresher_v4 => {
                    return Error::task_finished(&"bucket_refresher_v4", err);
                },
                err = &mut pinger_v6 => {
                    return Error::task_finished(&"pinger_v6", err);
                },
                err = &mut bucket_refresher_v6 => {
                    return Error::task_finished(&"bucket_refresher_v6", err);
                },
                err = &mut response_reader => {
                    return Error::task_finished(&"response_reader", err);
                }
            }
        }
    }
}

#[derive(Default)]
pub struct DhtConfig<'a> {
    pub peer_id: Option<Id20>,
    pub bootstrap_addrs: Option<Vec<String>>,
    pub routing_table: Option<RoutingTable>,
    pub routing_table_v6: Option<RoutingTable>,
    pub listen_addr: Option<SocketAddr>,
    pub peer_store: Option<PeerStore>,
    pub cancellation_token: Option<CancellationToken>,
    pub bind_device: Option<&'a BindDevice>,
    /// BEP 42 enforcement: nodes whose id does not match their address are kept out of the
    /// routing table, are not handed out in `nodes` and never get our announces or puts. Their
    /// answers still help the lookup in progress. Off by default, like in libtorrent.
    pub enforce_node_id: bool,
}

impl DhtState {
    pub async fn new() -> crate::Result<Arc<Self>> {
        Self::with_config(DhtConfig::default()).await
    }
    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancellation_token
    }

    #[inline(never)]
    pub fn with_config<'a>(mut config: DhtConfig<'a>) -> BoxFuture<'a, crate::Result<Arc<Self>>> {
        async move {
            let addr = config
                .listen_addr
                .unwrap_or((Ipv6Addr::UNSPECIFIED, 0).into());
            let socket = UdpSocket::bind_udp(
                addr,
                librqbit_dualstack_sockets::BindOpts {
                    request_dualstack: true,
                    reuseport: false,
                    device: config.bind_device,
                },
            )
            .map_err(|e| Error::Bind(Box::new(e)))?;

            let listen_addr = socket.bind_addr();
            info!("DHT listening on {:?}", listen_addr);

            let peer_id = config
                .peer_id
                .unwrap_or_else(|| generate_azereus_style(*b"rQ", crate_version!()));
            info!("starting up DHT with peer id {:?}", peer_id);
            let bootstrap_addrs = config
                .bootstrap_addrs
                .unwrap_or_else(|| crate::DHT_BOOTSTRAP.iter().map(|v| v.to_string()).collect());

            let token = config.cancellation_token.take().unwrap_or_default();

            let (in_tx, in_rx) = unbounded_channel();
            let state = Arc::new(Self::new_internal(
                peer_id,
                in_tx,
                config.routing_table,
                config.routing_table_v6,
                listen_addr,
                config.peer_store.unwrap_or_else(|| PeerStore::new(peer_id)),
                config.enforce_node_id,
                token,
            ));

            spawn_with_cancel(
                debug_span!("dht"),
                "dht",
                state.cancellation_token.clone(),
                {
                    let state = state.clone();
                    async move {
                        let worker = DhtWorker { socket, dht: state };
                        worker.start(in_rx, &bootstrap_addrs).await
                    }
                },
            );
            Ok(state)
        }
        .boxed()
    }

    pub fn get_peers(
        self: &Arc<Self>,
        info_hash: Id20,
        announce_port: Option<u16>,
    ) -> RequestPeersStream {
        RequestPeersStream::new(self.clone(), info_hash, announce_port)
    }

    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    pub fn stats(&self) -> DhtStats {
        self.get_stats()
    }

    pub fn with_routing_tables<R, F: FnOnce(&RoutingTable, &RoutingTable) -> R>(&self, f: F) -> R {
        f(&self.routing_table_v4.read(), &self.routing_table_v6.read())
    }

    // pub fn clone_routing_table(&self) -> RoutingTable {
    //     self.routing_table.read().clone()
    // }
}

trait FromSocketAddr: Sized {
    fn from_socket_addr(addr: SocketAddr) -> Option<Self>;
}

impl FromSocketAddr for SocketAddrV4 {
    fn from_socket_addr(addr: SocketAddr) -> Option<Self> {
        match addr {
            SocketAddr::V4(a) => Some(a),
            _ => None,
        }
    }
}

impl FromSocketAddr for SocketAddrV6 {
    fn from_socket_addr(addr: SocketAddr) -> Option<Self> {
        match addr {
            SocketAddr::V6(a) => Some(a),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
        str::FromStr,
        sync::Arc,
        time::Duration,
    };

    use bencode::ByteBufOwned;
    use bytes::Bytes;
    use librqbit_core::hash_id::Id20;
    use tokio::net::UdpSocket;

    use super::{DhtConfig, DhtState, PutArgs, Request, ResponseOrError, SignedMutable};
    use crate::{
        Dht, bep42, bep44,
        bprotocol::{self, FixedBytes, MessageKind, RawBencode},
    };

    async fn start_dht(bootstrap_addr: SocketAddr) -> Dht {
        DhtState::with_config(DhtConfig {
            listen_addr: Some((Ipv4Addr::LOCALHOST, 0).into()),
            bootstrap_addrs: Some(vec![bootstrap_addr.to_string()]),
            ..Default::default()
        })
        .await
        .unwrap()
    }

    async fn assert_answers_ping(dht: &Dht) {
        let client = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        client
            .send_to(
                b"d1:ad2:id20:abcdefghij0123456789e1:q4:ping1:t2:aa1:y1:qe",
                dht.listen_addr(),
            )
            .await
            .unwrap();
        let mut buf = [0u8; 1024];
        let len = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match client.recv_from(&mut buf).await {
                    Ok((len, addr)) if addr == dht.listen_addr() => return len,
                    _ => continue,
                }
            }
        })
        .await
        .expect("the DHT stopped answering");
        let response = bprotocol::deserialize_message::<ByteBufOwned>(&buf[..len]).unwrap();
        assert!(matches!(response.kind, MessageKind::Response(_)));
    }

    // On Windows, an ICMP port unreachable that comes back for an earlier send_to makes the next
    // recv_from on the same UDP socket fail with WSAECONNRESET (os error 10054).
    #[tokio::test]
    async fn test_dht_answers_after_icmp_port_unreachable() {
        // Nothing listens on this port once the socket is dropped.
        let closed = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap();
        let dht = start_dht(closed).await;

        // Let the bootstrap query reach the closed port and the ICMP error come back.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_answers_ping(&dht).await;
    }

    // On Windows, a datagram longer than the receive buffer makes recv_from fail with
    // WSAEMSGSIZE (os error 10040).
    #[tokio::test]
    async fn test_dht_answers_after_oversized_datagram() {
        // Stays open, so the bootstrap query gets neither an answer nor an ICMP error.
        let silent = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let dht = start_dht(silent.local_addr().unwrap()).await;

        // Some platforms refuse to send a datagram this large, which is fine.
        let _ = silent.send_to(&[0u8; 20_000], dht.listen_addr()).await;
        assert_answers_ping(&dht).await;
    }

    #[tokio::test]
    async fn test_external_ip_vote_moves_node_id() {
        let silent = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let dht = start_dht(silent.local_addr().unwrap()).await;
        let before = dht.get_stats();
        assert_eq!(before.external_ip_v4, None);
        assert_eq!(before.external_ip_v6, None);

        // Few enough nodes to all fit in the table whatever our id is. They all point at the
        // silent socket, so the lookup around the new id goes nowhere.
        let known = (0..8u8)
            .map(|i| Id20::new([i.wrapping_mul(31); 20]))
            .collect::<HashSet<_>>();
        for id in &known {
            dht.routing_table_v4
                .write()
                .add_node(*id, silent.local_addr().unwrap());
        }

        let ours_v4 = IpAddr::from(Ipv4Addr::new(124, 31, 75, 21));
        dht.vote_external_ip(ours_v4);
        let after_v4 = dht.get_stats();
        assert_eq!(
            after_v4.external_ip_v4,
            Some(Ipv4Addr::new(124, 31, 75, 21))
        );
        assert_ne!(after_v4.id, before.id);
        assert!(bep42::node_id_matches_ip(&after_v4.id, ours_v4));
        // The IPv6 side has not moved.
        assert_eq!(after_v4.id_v6, before.id_v6);
        assert_eq!(after_v4.external_ip_v6, None);

        // The table is rebuilt around the new id with the nodes we knew, and the peer store
        // measures distances from the new id.
        {
            let table = dht.routing_table_v4.read();
            assert_eq!(table.id(), after_v4.id);
            assert_eq!(table.iter().map(|n| n.id()).collect::<HashSet<_>>(), known);
        }
        assert_eq!(dht.peer_store.self_id(true), after_v4.id);
        assert_eq!(dht.peer_store.self_id(false), before.id_v6);

        let ours_v6 = IpAddr::from(Ipv6Addr::new(0x2001, 0x0db8, 0xaaaa, 0, 0, 0, 0, 1));
        dht.vote_external_ip(ours_v6);
        let after_v6 = dht.get_stats();
        assert_eq!(
            after_v6.external_ip_v6,
            Some(Ipv6Addr::new(0x2001, 0x0db8, 0xaaaa, 0, 0, 0, 0, 1))
        );
        assert!(bep42::node_id_matches_ip(&after_v6.id_v6, ours_v6));
        assert_eq!(after_v6.id, after_v4.id);
        assert_eq!(dht.routing_table_v6.read().id(), after_v6.id_v6);
        assert_eq!(dht.peer_store.self_id(false), after_v6.id_v6);
    }

    #[tokio::test]
    async fn test_external_ip_vote_keeps_matching_id() {
        let silent = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let ours = IpAddr::from(Ipv4Addr::new(21, 75, 31, 124));
        let id = bep42::generate_node_id(ours);
        let dht = DhtState::with_config(DhtConfig {
            peer_id: Some(id),
            listen_addr: Some((Ipv4Addr::LOCALHOST, 0).into()),
            bootstrap_addrs: Some(vec![silent.local_addr().unwrap().to_string()]),
            ..Default::default()
        })
        .await
        .unwrap();
        dht.vote_external_ip(ours);
        let stats = dht.get_stats();
        assert_eq!(stats.external_ip_v4, Some(Ipv4Addr::new(21, 75, 31, 124)));
        assert_eq!(stats.id, id);
    }

    #[tokio::test]
    async fn test_one_liar_does_not_move_node_id() {
        let silent = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let dht = start_dht(silent.local_addr().unwrap()).await;
        let before = dht.get_stats().id;
        for _ in 0..20 {
            dht.on_external_ip_reported(
                SocketAddr::from(([5, 6, 7, 8], 6881)),
                None,
                SocketAddr::from(([124, 31, 75, 21], 4321)),
            );
        }
        let stats = dht.get_stats();
        assert_eq!(stats.external_ip_v4, None);
        assert_eq!(stats.id, before);
    }

    #[tokio::test]
    async fn test_unsolicited_answers_do_not_vote() {
        let silent = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let dht = start_dht(silent.local_addr().unwrap()).await;
        // Answers to queries we never sent, from many networks: none of them counts.
        for n in 1..=20u8 {
            let msg = bprotocol::Message {
                transaction_id: ByteBufOwned::from(&[n, n][..]),
                version: None,
                ip: Some(SocketAddr::from(([124, 31, 75, 21], 4321))),
                kind: MessageKind::Response(bprotocol::Response {
                    id: Id20::new([n; 20]),
                    ..Default::default()
                }),
            };
            assert!(matches!(
                dht.on_received_message(msg, SocketAddr::from(([n, 1, 1, 1], 6881))),
                Err(crate::Error::RequestNotFound)
            ));
        }
        assert_eq!(dht.get_stats().external_ip_v4, None);
    }

    #[tokio::test]
    async fn test_dropped_request_leaves_no_inflight_entry() {
        let silent = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let dht = start_dht(silent.local_addr().unwrap()).await;
        let other = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let other_addr = other.local_addr().unwrap();
        let pending = |dht: &DhtState| {
            dht.inflight_by_transaction_id
                .iter()
                .filter(|e| e.key().1 == other_addr)
                .count()
        };
        // Nobody answers; the caller gives up long before RESPONSE_TIMEOUT, as a lookup past
        // its deadline does.
        let r = tokio::time::timeout(
            Duration::from_millis(100),
            dht.request(Request::Ping, other_addr),
        )
        .await;
        assert!(r.is_err());
        assert_eq!(pending(&dht), 0);
        // The query did go out (and was tracked while it was pending).
        let mut buf = [0u8; 1500];
        let (len, _) = other.recv_from(&mut buf).await.unwrap();
        assert!(bprotocol::deserialize_message::<ByteBufOwned>(&buf[..len]).is_ok());
    }

    #[tokio::test]
    async fn test_enforce_node_id_keeps_invalid_nodes_out() {
        let silent = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let dht = DhtState::with_config(DhtConfig {
            listen_addr: Some((Ipv4Addr::LOCALHOST, 0).into()),
            bootstrap_addrs: Some(vec![silent.local_addr().unwrap().to_string()]),
            enforce_node_id: true,
            ..Default::default()
        })
        .await
        .unwrap();

        let valid_addr = SocketAddr::from(([124, 31, 75, 21], 6881));
        let valid = bep42::generate_node_id(valid_addr.ip());
        let invalid_addr = SocketAddr::from(([21, 75, 31, 124], 6881));
        let mut invalid = valid;
        invalid.0[0] ^= 0xff;
        assert!(!bep42::is_valid_node_id(&invalid, invalid_addr.ip()));
        // Local addresses are exempt, whatever the id.
        let local_addr = SocketAddr::from(([192, 168, 1, 2], 6881));
        let local = Id20::new([7; 20]);

        let req = super::RecursiveRequest {
            max_depth: 4,
            info_hash: valid,
            request: super::Request::FindNode(valid),
            dht: dht.clone(),
            useful_nodes_limit: 32,
            useful_nodes: parking_lot::RwLock::new(Vec::new()),
            peer_tx: tokio::sync::mpsc::unbounded_channel().0,
            node_tx: tokio::sync::mpsc::unbounded_channel().0,
            callbacks: super::RecursiveRequestCallbacksFindNodes {},
        };
        for (id, addr) in [
            (valid, valid_addr),
            (invalid, invalid_addr),
            (local, local_addr),
        ] {
            super::RecursiveRequestCallbacks::on_request_start(&req.callbacks, &req, id, addr);
        }
        let in_table = |dht: &Dht| {
            dht.routing_table_v4
                .read()
                .iter()
                .map(|n| n.id())
                .collect::<HashSet<_>>()
        };
        assert_eq!(in_table(&dht), HashSet::from([valid, local]));

        // A node learned while enforcement was off is not handed out once it is on.
        dht.routing_table_v4.write().add_node(invalid, invalid_addr);
        let handed_out = |dht: &Dht| {
            let (nodes, _) = dht.generate_compact_nodes_both(valid, super::Want::V4);
            nodes.unwrap().iter().map(|n| n.id).collect::<HashSet<_>>()
        };
        assert_eq!(handed_out(&dht), HashSet::from([valid, local]));

        dht.set_enforce_node_id(false);
        assert_eq!(handed_out(&dht), HashSet::from([valid, invalid, local]));
    }

    #[tokio::test]
    async fn test_enforce_node_id_storage_targets() {
        let silent = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let dht = start_dht(silent.local_addr().unwrap()).await;
        let addr = SocketAddr::from(([124, 31, 75, 21], 6881));
        let valid = bep42::generate_node_id(addr.ip());
        let mut other_valid = bep42::generate_node_id(addr.ip());
        // Make sure the two differ in a byte BEP 42 leaves to chance.
        other_valid.0[10] = !valid.0[10];
        assert!(bep42::is_valid_node_id(&other_valid, addr.ip()));
        let mut invalid = valid;
        invalid.0[0] ^= 0xff;
        let target = Id20::new([0x42; 20]);

        // Off: whatever the node is listed under or answers with.
        assert!(dht.storage_target_acceptable(target, valid, addr));
        assert!(dht.storage_target_acceptable(invalid, invalid, addr));

        dht.set_enforce_node_id(true);
        assert!(dht.storage_target_acceptable(valid, valid, addr));
        // Listed next to the target, answering with an id valid for its address: refused.
        assert!(!dht.storage_target_acceptable(target, valid, addr));
        assert!(!dht.storage_target_acceptable(valid, other_valid, addr));
        assert!(!dht.storage_target_acceptable(invalid, invalid, addr));
        // Local addresses stay exempt, but the ids must still agree.
        let local = SocketAddr::from(([192, 168, 1, 2], 6881));
        assert!(dht.storage_target_acceptable(invalid, invalid, local));
        assert!(!dht.storage_target_acceptable(target, invalid, local));

        // The BEP 44 lookup collects put targets through the same check.
        let req = super::RecursiveRequest {
            max_depth: 4,
            info_hash: target,
            request: super::Request::Get(target),
            dht: dht.clone(),
            useful_nodes_limit: 32,
            useful_nodes: parking_lot::RwLock::new(Vec::new()),
            peer_tx: tokio::sync::mpsc::unbounded_channel().0,
            node_tx: tokio::sync::mpsc::unbounded_channel().0,
            callbacks: super::RecursiveRequestCallbacksGetItem {
                query: super::ItemQuery::Immutable,
                found: Default::default(),
                immutable_found: tokio::sync::Notify::new(),
            },
        };
        let answer = |id: Id20| {
            Ok(ResponseOrError::Response(Box::new(bprotocol::Response {
                id,
                token: Some(ByteBufOwned::from(&b"tokn"[..])),
                ..Default::default()
            })))
        };
        for (listed, answered) in [(target, valid), (valid, valid)] {
            super::RecursiveRequestCallbacks::on_request_end(
                &req.callbacks,
                &req,
                listed,
                addr,
                &answer(answered),
            );
        }
        let found = req.callbacks.found.lock();
        assert_eq!(found.put_targets.len(), 1);
        assert_eq!(found.put_targets[0].distance, valid.distance(&target));
    }

    #[tokio::test]
    async fn test_add_node_pings_and_inserts() {
        let silent = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let a = start_dht(silent.local_addr().unwrap()).await;
        let b = start_dht(silent.local_addr().unwrap()).await;
        assert_eq!(a.get_stats().routing_table_size, 0);

        // Given as an IPv4-mapped address, it still lands in the IPv4 table under its plain
        // address.
        let b_addr = b.listen_addr();
        let mapped = SocketAddr::new(
            match b_addr.ip() {
                IpAddr::V4(ip) => IpAddr::V6(ip.to_ipv6_mapped()),
                other => other,
            },
            b_addr.port(),
        );
        let id = a.add_node(mapped).await.unwrap();
        assert_eq!(id, b.get_stats().id);
        let in_table: Vec<_> = a
            .routing_table_v4
            .read()
            .iter()
            .map(|n| (n.id(), n.addr()))
            .collect();
        assert_eq!(in_table, vec![(id, b.listen_addr())]);
    }

    /// `n` DHT nodes on loopback that all know each other. They bootstrap from a socket that
    /// never answers, so their routing tables hold exactly the nodes added here.
    async fn start_mesh(n: usize) -> (Vec<Dht>, UdpSocket) {
        let silent = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let mut nodes = Vec::new();
        for _ in 0..n {
            nodes.push(start_dht(silent.local_addr().unwrap()).await);
        }
        for a in &nodes {
            for b in &nodes {
                if !Arc::ptr_eq(a, b) {
                    let id = b.get_stats().id;
                    a.routing_table_v4.write().add_node(id, b.listen_addr());
                }
            }
        }
        (nodes, silent)
    }

    fn rejection_codes(e: crate::Error) -> HashSet<i32> {
        match e {
            crate::Error::ItemNotStored { rejections, .. } => {
                rejections.into_iter().map(|(code, _)| code).collect()
            }
            other => panic!("expected ItemNotStored, got {other:#}"),
        }
    }

    #[tokio::test]
    async fn test_bep44_immutable_put_get_across_nodes() {
        let (nodes, _silent) = start_mesh(5).await;
        let value = Bytes::from_static(b"12:Hello World!");
        let target = nodes[0].put_immutable(value.clone()).await.unwrap();
        assert_eq!(
            target,
            Id20::from_str("e5f96f6f38320f0f33959cb4d3d656452117aadb").unwrap()
        );
        // Node 3 has a copy; node 0 does not keep what it puts, so it has to ask the others.
        assert_eq!(
            nodes[3].get_immutable(target).await.unwrap(),
            Some(value.clone())
        );
        assert_eq!(nodes[0].get_immutable(target).await.unwrap(), Some(value));
        assert!(
            nodes
                .iter()
                .skip(1)
                .all(|n| n.get_stats().stored_items == 1)
        );
        assert_eq!(nodes[0].get_stats().stored_items, 0);

        // Nobody has this one.
        let missing = bep44::immutable_target(b"3:abc");
        assert_eq!(nodes[2].get_immutable(missing).await.unwrap(), None);

        // What we refuse before sending anything.
        assert!(matches!(
            nodes[0]
                .put_immutable(Bytes::from_static(b"3:abc3:def"))
                .await,
            Err(crate::Error::InvalidItem(_))
        ));
    }

    #[tokio::test]
    async fn test_bep44_mutable_put_get_across_nodes() {
        let (nodes, _silent) = start_mesh(5).await;
        let key = [7u8; 32];
        let public_key = bep44::public_key(&key);
        let salt = b"foobar";

        nodes[0]
            .put_mutable(&key, salt, 1, Bytes::from_static(b"5:first"), None)
            .await
            .unwrap();
        let got = nodes[2]
            .get_mutable(public_key, salt)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((got.seq, &got.value[..]), (1, &b"5:first"[..]));
        assert_eq!(got.public_key, public_key);
        // Another salt is another item.
        assert_eq!(nodes[2].get_mutable(public_key, b"").await.unwrap(), None);

        // A newer version replaces it, if what the nodes have is the version "cas" names. (Put
        // from node 0 again: node 1 would leave its own copy of version 1 behind.)
        nodes[0]
            .put_mutable(&key, salt, 2, Bytes::from_static(b"6:second"), Some(1))
            .await
            .unwrap();
        let got = nodes[4]
            .get_mutable(public_key, salt)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((got.seq, &got.value[..]), (2, &b"6:second"[..]));

        // An older one is refused.
        let e = nodes[0]
            .put_mutable(&key, salt, 1, Bytes::from_static(b"5:older"), None)
            .await
            .unwrap_err();
        assert_eq!(rejection_codes(e), HashSet::from([302]));

        // So is one whose "cas" is not the version the nodes have.
        let e = nodes[0]
            .put_mutable(&key, salt, 3, Bytes::from_static(b"5:third"), Some(1))
            .await
            .unwrap_err();
        assert_eq!(rejection_codes(e), HashSet::from([301]));

        let got = nodes[3]
            .get_mutable(public_key, salt)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((got.seq, &got.value[..]), (2, &b"6:second"[..]));
    }

    #[tokio::test]
    async fn test_bep44_forged_signature_is_refused_and_ignored() {
        let (nodes, _silent) = start_mesh(3).await;
        let key = [9u8; 32];
        let public_key = bep44::public_key(&key);
        let target = bep44::mutable_target(&public_key, b"");
        nodes[0]
            .put_mutable(&key, b"", 2, Bytes::from_static(b"4:real"), None)
            .await
            .unwrap();

        // A node refuses a put whose signature does not verify (206).
        let victim = nodes[1].listen_addr();
        let token = match nodes[0]
            .request(Request::Get(target), victim)
            .await
            .unwrap()
        {
            ResponseOrError::Response(r) => r.token.unwrap(),
            ResponseOrError::Error(e) => panic!("get failed: {e:?}"),
        };
        let value = Bytes::from_static(b"6:forged");
        let mut signature = bep44::sign_mutable(&key, b"", 5, &value);
        signature[0] ^= 1;
        let forged = PutArgs {
            token,
            value: value.clone(),
            mutable: Some(SignedMutable {
                public_key,
                signature,
                seq: 5,
                salt: Bytes::new(),
                cas: None,
            }),
        };
        match nodes[0]
            .request(Request::Put(Box::new(forged)), victim)
            .await
            .unwrap()
        {
            ResponseOrError::Error(e) => assert_eq!(e.code, 206),
            ResponseOrError::Response(r) => panic!("forged put accepted: {r:?}"),
        }

        // A node that answers `get` with a newer but forged version is ignored by the lookup.
        let liar = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let liar_addr = liar.local_addr().unwrap();
        let liar_id = Id20::new([0x42; 20]);
        let liar_task = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            loop {
                let Ok((len, from)) = liar.recv_from(&mut buf).await else {
                    return;
                };
                let Ok(msg) = bprotocol::deserialize_message::<ByteBufOwned>(&buf[..len]) else {
                    continue;
                };
                if !matches!(msg.kind, MessageKind::GetRequest(_)) {
                    continue;
                }
                let response = bprotocol::Response {
                    id: liar_id,
                    v: Some(RawBencode(ByteBufOwned::from(value.clone()))),
                    k: Some(FixedBytes(public_key)),
                    sig: Some(FixedBytes(signature)),
                    seq: Some(5),
                    ..Default::default()
                };
                let mut out = Vec::new();
                bprotocol::serialize_message(
                    &mut out,
                    msg.transaction_id,
                    None,
                    Some(from),
                    MessageKind::Response(response),
                )
                .unwrap();
                liar.send_to(&out, from).await.unwrap();
            }
        });
        nodes[2]
            .routing_table_v4
            .write()
            .add_node(liar_id, liar_addr);
        let got = nodes[2]
            .get_mutable(public_key, b"")
            .await
            .unwrap()
            .unwrap();
        assert_eq!((got.seq, &got.value[..]), (2, &b"4:real"[..]));
        liar_task.abort();
    }

    /// Sends one query from `socket` to `to` and returns the answer.
    async fn raw_query(
        socket: &UdpSocket,
        to: SocketAddr,
        kind: MessageKind<ByteBufOwned>,
    ) -> bprotocol::Message<ByteBufOwned> {
        let mut out = Vec::new();
        bprotocol::serialize_message(&mut out, ByteBufOwned::from(&b"qq"[..]), None, None, kind)
            .unwrap();
        socket.send_to(&out, to).await.unwrap();
        raw_answer(socket).await
    }

    async fn raw_answer(socket: &UdpSocket) -> bprotocol::Message<ByteBufOwned> {
        let mut buf = [0u8; 2048];
        let (len, _) = tokio::time::timeout(Duration::from_secs(10), socket.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        bprotocol::deserialize_message::<ByteBufOwned>(&buf[..len]).unwrap()
    }

    /// Asks `to` for a write token for `target`, from `from`.
    async fn token_for(from: &Dht, to: SocketAddr, target: Id20) -> ByteBufOwned {
        match from.request(Request::Get(target), to).await.unwrap() {
            ResponseOrError::Response(r) => r.token.unwrap(),
            ResponseOrError::Error(e) => panic!("get failed: {e:?}"),
        }
    }

    /// The KRPC error code `to` answers the put `args` from `from` with, 0 if it stored it.
    async fn put_code(from: &Dht, to: SocketAddr, args: PutArgs) -> i32 {
        match from
            .request(Request::Put(Box::new(args)), to)
            .await
            .unwrap()
        {
            ResponseOrError::Response(_) => 0,
            ResponseOrError::Error(e) => e.code,
        }
    }

    #[tokio::test]
    async fn test_bep44_put_errors_over_the_wire() {
        let (nodes, _silent) = start_mesh(2).await;
        let to = nodes[1].listen_addr();
        let value = Bytes::from_static(b"3:abc");
        let target = bep44::immutable_target(&value);
        let immutable = |token: ByteBufOwned, value: Bytes| PutArgs {
            token,
            value,
            mutable: None,
        };

        // No token from us, or one for another target: 203, and nothing is stored.
        let garbage = ByteBufOwned::from(&b"12345678"[..]);
        assert_eq!(
            put_code(&nodes[0], to, immutable(garbage, value.clone())).await,
            203
        );
        let other = token_for(&nodes[0], to, bep44::immutable_target(b"3:xyz")).await;
        assert_eq!(
            put_code(&nodes[0], to, immutable(other.clone(), value.clone())).await,
            203
        );
        assert_eq!(nodes[1].get_stats().stored_items, 0);
        // One token is good for its target only, but for it as often as needed.
        assert_eq!(
            put_code(
                &nodes[0],
                to,
                immutable(other.clone(), Bytes::from_static(b"3:xyz"))
            )
            .await,
            0
        );
        assert_eq!(
            put_code(
                &nodes[0],
                to,
                immutable(other, Bytes::from_static(b"3:xyz"))
            )
            .await,
            0
        );

        // 205: a value over 1000 bytes.
        let mut big = b"997:".to_vec();
        big.resize(1000 + 1, b'x');
        let big = Bytes::from(big);
        let token = token_for(&nodes[0], to, bep44::immutable_target(&big)).await;
        assert_eq!(put_code(&nodes[0], to, immutable(token, big)).await, 205);

        // 207: a salt over 64 bytes, on an otherwise good mutable put.
        let key = [3u8; 32];
        let public_key = bep44::public_key(&key);
        let salt = Bytes::from(vec![b's'; 65]);
        let token = token_for(&nodes[0], to, bep44::mutable_target(&public_key, &salt)).await;
        let args = PutArgs {
            token,
            value: value.clone(),
            mutable: Some(SignedMutable {
                public_key,
                signature: bep44::sign_mutable(&key, &salt, 1, &value),
                seq: 1,
                salt,
                cas: None,
            }),
        };
        assert_eq!(put_code(&nodes[0], to, args).await, 207);
        assert_eq!(nodes[1].get_stats().stored_items, 1);
        assert!(nodes[1].items.get(&target, super::now()).is_none());
    }

    #[tokio::test]
    async fn test_bep44_malformed_put_is_answered_with_203() {
        let silent = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let dht = start_dht(silent.local_addr().unwrap()).await;
        let client = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        // "sig" is 3 bytes instead of 64.
        client
            .send_to(
                b"d1:ad2:id20:abcdefghij01234567891:k32:abcdefghij0123456789abcdefghij013:seqi1e\
                  3:sig3:abc5:token4:abcd1:v3:abce1:q3:put1:t2:xy1:y1:qe",
                dht.listen_addr(),
            )
            .await
            .unwrap();
        let answer = raw_answer(&client).await;
        assert_eq!(answer.transaction_id.as_ref(), b"xy");
        match answer.kind {
            MessageKind::Error(e) => assert_eq!(e.code, 203),
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_bep44_get_with_seq_leaves_the_item_out() {
        let (nodes, _silent) = start_mesh(2).await;
        let key = [5u8; 32];
        let public_key = bep44::public_key(&key);
        nodes[0]
            .put_mutable(&key, b"", 4, Bytes::from_static(b"4:four"), None)
            .await
            .unwrap();
        let target = bep44::mutable_target(&public_key, b"");
        let client = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let get = |seq| {
            MessageKind::GetRequest(bprotocol::GetRequest {
                id: Id20::new([1; 20]),
                target,
                seq,
                want: None,
            })
        };
        let to = nodes[1].listen_addr();

        // The requester has version 4 already: only "seq" comes back.
        let r = match raw_query(&client, to, get(Some(4))).await.kind {
            MessageKind::Response(r) => r,
            other => panic!("expected a response, got {other:?}"),
        };
        assert_eq!(r.seq, Some(4));
        assert!(r.v.is_none() && r.k.is_none() && r.sig.is_none());
        assert!(r.token.is_some());

        // It has an older one: everything comes back.
        let r = match raw_query(&client, to, get(Some(3))).await.kind {
            MessageKind::Response(r) => r,
            other => panic!("expected a response, got {other:?}"),
        };
        assert_eq!(r.seq, Some(4));
        assert_eq!(r.v.unwrap().0.as_ref(), b"4:four");
        assert_eq!(r.k.unwrap().0, public_key);
        assert!(r.sig.is_some());
    }

    #[tokio::test]
    async fn test_bep44_equal_seq_and_cas_only_to_holders() {
        let (mut nodes, silent) = start_mesh(3).await;
        let key = [6u8; 32];
        let public_key = bep44::public_key(&key);
        let salt = b"s";
        let target = bep44::mutable_target(&public_key, salt);
        nodes[0]
            .put_mutable(&key, salt, 1, Bytes::from_static(b"5:first"), None)
            .await
            .unwrap();

        // The same version again refreshes it; another value under the same "seq" is refused.
        nodes[0]
            .put_mutable(&key, salt, 1, Bytes::from_static(b"5:first"), None)
            .await
            .unwrap();
        let e = nodes[0]
            .put_mutable(&key, salt, 1, Bytes::from_static(b"5:other"), None)
            .await
            .unwrap_err();
        assert_eq!(rejection_codes(e), HashSet::from([302]));

        // A newcomer that has no version of the item.
        let newcomer = start_dht(silent.local_addr().unwrap()).await;
        for n in &nodes {
            n.routing_table_v4
                .write()
                .add_node(newcomer.get_stats().id, newcomer.listen_addr());
            newcomer
                .routing_table_v4
                .write()
                .add_node(n.get_stats().id, n.listen_addr());
        }
        nodes.push(newcomer);

        // A wrong "cas" is refused by the nodes that have the item (301), but not sent to the
        // newcomer, which takes the new version.
        nodes[0]
            .put_mutable(&key, salt, 2, Bytes::from_static(b"6:second"), Some(7))
            .await
            .unwrap();
        let version = |n: &Dht| match n.items.get(&target, super::now()) {
            Some(bep44::StoredView::Mutable(item)) => item.seq,
            other => panic!("expected a mutable item, got {other:?}"),
        };
        assert_eq!(version(&nodes[1]), 1);
        assert_eq!(version(&nodes[2]), 1);
        assert_eq!(version(&nodes[3]), 2);

        // Anyone can re-publish a version it found, without the key; not a tampered one.
        let found = nodes[3]
            .get_mutable(public_key, salt)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.seq, 2);
        let mut tampered = found.clone();
        tampered.value = Bytes::from_static(b"6:third!");
        assert!(matches!(
            nodes[1].put_mutable_signed(tampered, salt, None).await,
            Err(crate::Error::InvalidItem(_))
        ));
        nodes[1]
            .put_mutable_signed(found, salt, None)
            .await
            .unwrap();
        assert_eq!(version(&nodes[2]), 2);
    }

    #[tokio::test]
    async fn test_bep44_immutable_get_stops_at_the_first_checked_value() {
        let (nodes, silent) = start_mesh(3).await;
        let value = Bytes::from_static(b"12:Hello World!");
        let target = nodes[0].put_immutable(value.clone()).await.unwrap();

        // A value that does not hash to the target is ignored.
        let liar = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let liar_addr = liar.local_addr().unwrap();
        let liar_task = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            loop {
                let Ok((len, from)) = liar.recv_from(&mut buf).await else {
                    return;
                };
                let Ok(msg) = bprotocol::deserialize_message::<ByteBufOwned>(&buf[..len]) else {
                    continue;
                };
                let response = bprotocol::Response {
                    id: Id20::new([0x42; 20]),
                    v: Some(RawBencode(ByteBufOwned::from(&b"5:wrong"[..]))),
                    ..Default::default()
                };
                let mut out = Vec::new();
                bprotocol::serialize_message(
                    &mut out,
                    msg.transaction_id,
                    None,
                    Some(from),
                    MessageKind::Response(response),
                )
                .unwrap();
                liar.send_to(&out, from).await.unwrap();
            }
        });
        let missing = bep44::immutable_target(b"3:abc");
        nodes[1]
            .routing_table_v4
            .write()
            .add_node(Id20::new([0x42; 20]), liar_addr);
        assert_eq!(nodes[1].get_immutable(missing).await.unwrap(), None);

        // A node that never answers is one of the lookup's seeds; the value from the others is
        // final, so the call does not wait RESPONSE_TIMEOUT for it.
        nodes[0]
            .routing_table_v4
            .write()
            .add_node(target, silent.local_addr().unwrap());
        let got = tokio::time::timeout(crate::RESPONSE_TIMEOUT / 2, nodes[0].get_immutable(target))
            .await
            .expect("get_immutable waited for the silent node")
            .unwrap();
        assert_eq!(got, Some(value));

        liar_task.abort();
    }

    #[tokio::test]
    async fn test_bep44_get_mutable_without_nodes_uses_what_we_store() {
        let silent = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let dht = start_dht(silent.local_addr().unwrap()).await;
        let key = [8u8; 32];
        let public_key = bep44::public_key(&key);
        let value = Bytes::from_static(b"4:mine");
        let target = bep44::mutable_target(&public_key, b"");
        // Put to it as another node would; its routing table stays empty.
        let from = SocketAddr::from(([127, 0, 0, 1], 9));
        let req = bprotocol::PutRequest {
            id: Id20::new([1; 20]),
            token: ByteBufOwned::from(
                &dht.write_tokens.token(from.ip(), &target, super::now())[..],
            ),
            v: RawBencode(ByteBufOwned::from(value.clone())),
            k: Some(FixedBytes(public_key)),
            sig: Some(FixedBytes(bep44::sign_mutable(&key, b"", 1, &value))),
            seq: Some(1),
            salt: None,
            cas: None,
        };
        dht.store_item(&req, from).unwrap();
        assert_eq!(dht.get_stats().routing_table_size, 0);
        let got = dht.get_mutable(public_key, b"").await.unwrap().unwrap();
        assert_eq!((got.seq, got.value), (1, value));
        // Without a stored item, there is nothing to fall back on.
        assert!(dht.get_mutable(public_key, b"x").await.is_err());
    }
}
