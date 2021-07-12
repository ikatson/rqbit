use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
};

use crate::{
    bprotocol::{
        self, CompactNodeInfo, FindNodeRequest, GetPeersRequest, Message, MessageKind, Node,
    },
    routing_table::{InsertResult, RoutingTable},
    DHT_BOOTSTRAP,
};
use anyhow::Context;
use bencode::ByteString;
use futures::{stream::FuturesUnordered, StreamExt};
use librqbit_core::{id20::Id20, peer_id::generate_peer_id};
use log::{debug, info, trace, warn};
use parking_lot::Mutex;
use tokio::{
    net::UdpSocket,
    sync::mpsc::{
        channel, unbounded_channel, Receiver, Sender, UnboundedReceiver, UnboundedSender,
    },
};
use tokio_stream::wrappers::UnboundedReceiverStream;

struct OutstandingRequest {
    transaction_id: u16,
    addr: SocketAddr,
    request: Request,
}

struct DhtState {
    id: Id20,
    next_transaction_id: u16,
    outstanding_requests: Vec<OutstandingRequest>,
    searching_for_peers: Vec<Id20>,
    routing_table: RoutingTable,
    sender: UnboundedSender<(Message<ByteString>, SocketAddr)>,

    // TODO: convert to broadcast
    subscribers: HashMap<Id20, Vec<UnboundedSender<Response>>>,

    made_requests: HashSet<(Request, SocketAddr)>,
}

impl DhtState {
    pub fn new(id: Id20, sender: UnboundedSender<(Message<ByteString>, SocketAddr)>) -> Self {
        Self {
            id,
            next_transaction_id: 0,
            outstanding_requests: Vec::new(),
            searching_for_peers: Vec::new(),
            routing_table: RoutingTable::new(id),
            sender,
            subscribers: Default::default(),
            made_requests: Default::default(),
        }
    }

    pub fn create_request(&mut self, request: Request, addr: SocketAddr) -> Message<ByteString> {
        let transaction_id = self.next_transaction_id;
        let transaction_id_buf = [(transaction_id >> 8) as u8, (transaction_id & 0xff) as u8];
        self.next_transaction_id += 1;
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
        self.outstanding_requests.push(OutstandingRequest {
            transaction_id,
            addr,
            request,
            // time: Instant::now(),
        });
        message
    }
    fn on_incoming_from_remote(
        &mut self,
        msg: Message<ByteString>,
        addr: SocketAddr,
    ) -> anyhow::Result<()> {
        match &msg.kind {
            MessageKind::Error(_) | MessageKind::Response(_) => {}
            MessageKind::PingRequest(_) => {
                let response = bprotocol::Response {
                    id: self.id,
                    nodes: None,
                    values: None,
                    token: None,
                };
                let message = Message {
                    transaction_id: msg.transaction_id,
                    version: None,
                    ip: None,
                    kind: MessageKind::Response(response),
                };
                self.sender.send((message, addr))?;
                return Ok(());
            }
            MessageKind::FindNodeRequest(_) | MessageKind::GetPeersRequest(_) => {
                let target = match &msg.kind {
                    MessageKind::FindNodeRequest(req) => req.target,
                    MessageKind::GetPeersRequest(req) => req.info_hash,
                    _ => unreachable!(),
                };
                // we don't track who is downloading a torrent (we don't have a peer store),
                // so send nodes all the time.
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
                let compact_node_info = CompactNodeInfo { nodes };
                let response = bprotocol::Response {
                    id: self.id,
                    nodes: Some(compact_node_info),
                    values: None,
                    token: None,
                };
                let message = Message {
                    transaction_id: msg.transaction_id,
                    version: None,
                    ip: None,
                    kind: MessageKind::Response(response),
                };
                self.sender.send((message, addr))?;
                return Ok(());
            }
        };
        if msg.transaction_id.len() != 2 {
            anyhow::bail!(
                "{}: transaction id unrecognized, we didn't ask for it. Message: {:?}",
                addr,
                msg
            )
        }
        let tid = ((msg.transaction_id[0] as u16) << 8) + (msg.transaction_id[1] as u16);
        // O(n) but whatever
        let outstanding_id = self
            .outstanding_requests
            .iter()
            .position(|req| req.transaction_id == tid && req.addr == addr)
            .ok_or_else(|| anyhow::anyhow!("outstanding request not found. Message: {:?}", msg))?;
        let outstanding = self.outstanding_requests.remove(outstanding_id);
        let response = match msg.kind {
            MessageKind::Error(e) => {
                anyhow::bail!(
                    "request {:?} received error response {:?}",
                    outstanding.request,
                    e
                )
            }
            MessageKind::Response(r) => r,
            _ => unreachable!(),
        };
        match outstanding.request {
            Request::FindNode(id) => {
                let nodes = response
                    .nodes
                    .ok_or_else(|| anyhow::anyhow!("expected nodes for find_node requests"))?;
                self.on_found_nodes(response.id, addr, id, nodes)
            }
            Request::GetPeers(id) => self.on_found_peers_or_nodes(response.id, addr, id, response),
        }
    }

    pub fn on_request(
        &mut self,
        request: Request,
        sender: UnboundedSender<Response>,
    ) -> anyhow::Result<()> {
        match request {
            Request::GetPeers(info_hash) => {
                let subs = self.subscribers.entry(info_hash).or_default();
                subs.push(sender);
                self.searching_for_peers.push(info_hash);

                // workaround borrow checker.
                let mut addrs = Vec::new();
                for node in self
                    .routing_table
                    .sorted_by_distance_from_mut(info_hash)
                    .into_iter()
                    .take(8)
                {
                    node.mark_outgoing_request();
                    addrs.push(node.addr());
                }
                for addr in addrs {
                    let request = self.create_request(Request::GetPeers(info_hash), addr);
                    self.sender
                        .send((request, addr))
                        .context("DhtState: error sending to self.sender")?;
                }
            }
            Request::FindNode(_) => todo!(),
        };
        Ok(())
    }

    fn on_found_nodes(
        &mut self,
        source: Id20,
        source_addr: SocketAddr,
        _target: Id20,
        nodes: CompactNodeInfo,
    ) -> anyhow::Result<()> {
        match self.routing_table.add_node(source, source_addr) {
            InsertResult::ReplacedBad(_) | InsertResult::Added => {
                for idx in 0..self.searching_for_peers.len() {
                    let info_hash = self.searching_for_peers[idx];
                    let request = Request::GetPeers(info_hash);
                    if self.made_requests.insert((request, source_addr)) {
                        self.routing_table.mark_outgoing_request(&source);
                        let msg = self.create_request(request, source_addr);
                        self.sender.send((msg, source_addr))?;
                    }
                }
            }
            InsertResult::WasExisting => {
                self.routing_table.mark_response(&source);
            }
            _ => {}
        };
        for node in nodes.nodes {
            match self.routing_table.add_node(node.id, node.addr.into()) {
                InsertResult::ReplacedBad(_) | InsertResult::Added => {
                    for idx in 0..self.searching_for_peers.len() {
                        let info_hash = self.searching_for_peers[idx];
                        let request = Request::GetPeers(info_hash);
                        if self.made_requests.insert((request, node.addr.into())) {
                            let msg =
                                self.create_request(Request::GetPeers(info_hash), node.addr.into());
                            self.routing_table.mark_outgoing_request(&node.id);
                            self.sender.send((msg, node.addr.into()))?
                        }
                    }
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
        if let Some(peers) = data.values {
            let subscribers = match self.subscribers.get(&target) {
                Some(subscribers) => subscribers,
                None => {
                    warn!(
                        "ignoring peers for {:?}: no subscribers left. Peers: {:?}",
                        target, peers
                    );
                    return Ok(());
                }
            };
            for subscriber in subscribers {
                for peer in peers.iter() {
                    subscriber.send(Response::Peer(peer.addr.into()))?
                }
            }
        };
        if let Some(nodes) = data.nodes {
            for node in nodes.nodes {
                self.routing_table.add_node(node.id, node.addr.into());
                let request = Request::GetPeers(target);
                if self.made_requests.insert((request, node.addr.into())) {
                    let msg = self.create_request(Request::GetPeers(target), node.addr.into());
                    self.routing_table.mark_outgoing_request(&node.id);
                    self.sender.send((msg, node.addr.into()))?
                }
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
                Err(e) => log::debug!("{}: error deserializing incoming message: {}", addr, e),
            }
        }
        Err::<(), _>(anyhow::anyhow!(
            "DHT UDP socket reader over, nowhere to read messages from"
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

#[derive(Debug)]
enum Response {
    Peer(SocketAddr),
}

pub struct Dht {
    request_tx: Sender<(Request, UnboundedSender<Response>)>,
}

struct DhtWorker {
    socket: UdpSocket,
    peer_id: Id20,
    state: Mutex<DhtState>,
}

impl DhtWorker {
    fn on_request(
        &self,
        request: Request,
        sender: UnboundedSender<Response>,
    ) -> anyhow::Result<()> {
        self.state.lock().on_request(request, sender)
    }
    fn on_response(&self, msg: Message<ByteString>, addr: SocketAddr) -> anyhow::Result<()> {
        self.state.lock().on_incoming_from_remote(msg, addr)
    }

    async fn start(
        self,
        in_tx: UnboundedSender<(Message<ByteString>, SocketAddr)>,
        in_rx: UnboundedReceiver<(Message<ByteString>, SocketAddr)>,
        mut request_rx: Receiver<(Request, UnboundedSender<Response>)>,
        bootstrap_addrs: &[String],
    ) -> anyhow::Result<()> {
        let (out_tx, mut out_rx) = channel(1);
        let framer = run_framer(&self.socket, in_rx, out_tx);

        let bootstrap = async {
            let mut futs = FuturesUnordered::new();
            // bootstrap
            for addr in bootstrap_addrs.iter() {
                let addr = addr;
                let this = &self;
                let in_tx = &in_tx;
                futs.push(async move {
                    match tokio::net::lookup_host(addr).await {
                        Ok(addrs) => {
                            for addr in addrs {
                                let request = this
                                    .state
                                    .lock()
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

        let request_reader = {
            let this = &self;
            async move {
                while let Some((request, sender)) = request_rx.recv().await {
                    this.on_request(request, sender)
                        .context("error processing request")?;
                }
                Err::<(), _>(anyhow::anyhow!(
                    "closed request reader, no more subscribers"
                ))
            }
        };

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
        tokio::pin!(request_reader);
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
                err = &mut request_reader => {anyhow::bail!("request reader quit: {:?}", err)}
                err = &mut response_reader => {anyhow::bail!("response reader quit: {:?}", err)}
            }
        }
    }
}

impl Dht {
    pub async fn new() -> anyhow::Result<Self> {
        Self::with_bootstrap_addrs(DHT_BOOTSTRAP).await
    }
    pub async fn with_bootstrap_addrs(bootstrap_addrs: &[&str]) -> anyhow::Result<Self> {
        let (request_tx, request_rx) = channel(1);
        let socket = UdpSocket::bind("0.0.0.0:0")
            .await
            .context("error binding socket")?;
        let peer_id = generate_peer_id();
        info!("starting up DHT with peer id {:?}", peer_id);
        let bootstrap_addrs = bootstrap_addrs
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();

        tokio::spawn(async move {
            let (in_tx, in_rx) = unbounded_channel();
            let worker = DhtWorker {
                socket,
                peer_id,
                state: Mutex::new(DhtState::new(peer_id, in_tx.clone())),
            };
            let result = worker
                .start(in_tx, in_rx, request_rx, &bootstrap_addrs)
                .await;
            warn!("DHT worker finished with {:?}", result);
        });
        Ok(Dht { request_tx })
    }
    pub async fn get_peers(&self, info_hash: Id20) -> impl StreamExt<Item = SocketAddr> {
        let (tx, rx) = unbounded_channel::<Response>();
        self.request_tx
            .send((Request::GetPeers(info_hash), tx))
            .await
            .unwrap();
        UnboundedReceiverStream::new(rx).map(|r| match r {
            Response::Peer(addr) => addr,
        })
    }
}
