use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    net::{SocketAddr, SocketAddrV4},
    time::Instant,
};

use bencode::ByteString;
use dht::{
    bprotocol::{
        self, CompactNodeInfo, CompactPeerInfo, FindNodeRequest, GetPeersRequest, Message,
        MessageKind,
    },
    id20::Id20,
    routing_table::RoutingTable,
};
use futures::{stream::FuturesUnordered, StreamExt};
use librqbit_core::peer_id::generate_peer_id;
use log::{debug, warn};
use parking_lot::Mutex;
use tokio::{
    net::UdpSocket,
    sync::mpsc::{channel, Receiver, Sender, UnboundedReceiver, UnboundedSender},
};
use tokio_stream::wrappers::ReceiverStream;

struct OutstandingRequest {
    transaction_id: u16,
    addr: SocketAddr,
    request: Request,
    time: Instant,
}

struct DhtState {
    id: Id20,
    next_transaction_id: u16,
    outstanding_requests: Vec<OutstandingRequest>,
    searching_for_peers: Vec<Id20>,
    routing_table: RoutingTable,
    sender: UnboundedSender<(Message<ByteString>, SocketAddr)>,

    // TODO: convert to broadcast
    subscribers: HashMap<Id20, Vec<Sender<Response>>>,
}

enum PeersOrNodes {
    Nodes(CompactNodeInfo),
    Peers(Vec<CompactPeerInfo>),
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
        }
    }

    fn add_searching_for_peers(&mut self, info_hash: Id20) {
        self.searching_for_peers.push(info_hash)
    }
    pub fn create_request(&mut self, request: Request, addr: SocketAddr) -> Message<ByteString> {
        let transaction_id = self.next_transaction_id;
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
        };
        self.outstanding_requests.push(OutstandingRequest {
            transaction_id,
            addr,
            request,
            time: Instant::now(),
        });
        message
    }
    fn on_incoming_from_remote(
        &mut self,
        msg: Message<ByteString>,
        addr: SocketAddr,
    ) -> anyhow::Result<()> {
        match msg.kind {
            MessageKind::Error(_) | MessageKind::Response(_) => {}
            other => anyhow::bail!("requests from DHT not supported, but got {:?}", other),
        };
        if msg.transaction_id.len() != 2 {
            anyhow::bail!("transaction id unrecognized")
        }
        let tid = ((msg.transaction_id[0] as u16) << 8) + (msg.transaction_id[1] as u16);
        // O(n) but whatever
        let outstanding_id = self
            .outstanding_requests
            .iter()
            .position(|req| req.transaction_id == tid && req.addr == addr)
            .ok_or_else(|| anyhow::anyhow!("outstanding request not found"))?;
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
            Request::GetPeers(id) => {
                if response.id != id {
                    anyhow::bail!(
                        "response id does not match: expected {:?}, received {:?}",
                        id,
                        response.id
                    )
                };
                let pn = match (response.nodes, response.values) {
                    (Some(nodes), None) => PeersOrNodes::Nodes(nodes),
                    (None, Some(peers)) => PeersOrNodes::Peers(peers),
                    _ => anyhow::bail!("expected nodes or values to be set in find_peers response"),
                };
                self.on_found_peers_or_nodes(response.id, addr, id, pn)
            }
        }
    }

    pub fn on_request(&mut self, request: Request, sender: Sender<Response>) -> anyhow::Result<()> {
        match request {
            Request::GetPeers(info_hash) => {
                let subs = self.subscribers.entry(info_hash).or_default();
                subs.push(sender);
                self.add_searching_for_peers(info_hash);

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
                    self.sender.send((request, addr))?;
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
        target: Id20,
        nodes: CompactNodeInfo,
    ) -> anyhow::Result<()> {
        todo!("on_found_nodes not implemented")
    }

    fn on_found_peers_or_nodes(
        &mut self,
        source: Id20,
        source_addr: SocketAddr,
        target: Id20,
        data: PeersOrNodes,
    ) -> anyhow::Result<()> {
        todo!("on_found_peers_or_nodes not implemented")
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
            buf.clear();
            bprotocol::serialize_message(
                &mut buf,
                msg.transaction_id,
                msg.version,
                msg.ip,
                msg.kind,
            )
            .unwrap();
            socket.send_to(&buf, addr).await.unwrap();
        }
    };
    let reader = async {
        let mut buf = vec![0u8; 16384];
        while let Ok((size, addr)) = socket.recv_from(&mut buf).await {
            match bprotocol::deserialize_message::<ByteString>(&buf[..size]) {
                Ok(msg) => match output_tx.send((msg, addr)).await {
                    Ok(_) => {}
                    Err(_) => break,
                },
                Err(e) => log::warn!("error deseriaizing msg: {}", e),
            }
        }
    };
    tokio::select! {
        _ = writer => {},
        _ = reader => {},
    };
    Ok(())
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

struct Dht {
    request_tx: Sender<(Request, Sender<Response>)>,
}

struct DhtWorker {
    socket: UdpSocket,
    peer_id: Id20,
    state: Mutex<DhtState>,
}

impl DhtWorker {
    fn on_request(&self, request: Request, sender: Sender<Response>) -> anyhow::Result<()> {
        self.state.lock().on_request(request, sender)
    }
    fn on_response(&self, msg: Message<ByteString>, addr: SocketAddr) -> anyhow::Result<()> {
        self.state.lock().on_incoming_from_remote(msg, addr)
    }

    async fn start(
        self,
        in_tx: UnboundedSender<(Message<ByteString>, SocketAddr)>,
        in_rx: UnboundedReceiver<(Message<ByteString>, SocketAddr)>,
        mut request_rx: Receiver<(Request, Sender<Response>)>,
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
                                match in_tx.send((request, addr)) {
                                    Ok(_) => {}
                                    Err(e) => {
                                        debug!("bootstrap: channel closed, did not send {:?}", e)
                                    }
                                };
                            }
                        }
                        Err(e) => warn!("error looking up {}", addr),
                    }
                });
            }
            while futs.next().await.is_some() {}
        };
        let mut bootstrap_done = false;

        let request_reader = {
            let this = &self;
            async move {
                while let Some((request, sender)) = request_rx.recv().await {
                    this.on_request(request, sender).unwrap();
                }
            }
        };

        let response_reader = {
            let this = &self;
            async move {
                while let Some((response, addr)) = out_rx.recv().await {
                    this.on_response(response, addr).unwrap();
                }
            }
        };

        tokio::pin!(framer);
        tokio::pin!(bootstrap);
        tokio::pin!(request_reader);
        tokio::pin!(response_reader);

        loop {
            tokio::select! {
                _ = &mut framer => {
                    anyhow::bail!("framer quit")
                },
                _ = &mut bootstrap, if !bootstrap_done => {
                    bootstrap_done = true
                },
                _ = &mut request_reader => {anyhow::bail!("request reader quit")}
                _ = &mut response_reader => {anyhow::bail!("response reader quit")}
            }
        }
    }
}

impl Dht {
    pub async fn new(bootstrap_addrs: &[&str]) -> anyhow::Result<Self> {
        let (request_tx, request_rx) = channel(1);
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        let peer_id = Id20(generate_peer_id());
        let bootstrap_addrs = bootstrap_addrs
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();

        tokio::spawn(async move {
            let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
            let worker = DhtWorker {
                socket,
                peer_id,
                state: Mutex::new(DhtState::new(peer_id, in_tx.clone())),
            };
            worker
                .start(in_tx, in_rx, request_rx, &bootstrap_addrs)
                .await
        });
        Ok(Dht { request_tx })
    }
    pub async fn get_peers(&self, info_hash: Id20) -> impl StreamExt<Item = SocketAddr> {
        let (tx, rx) = channel::<Response>(1);
        self.request_tx
            .send((Request::GetPeers(info_hash), tx))
            .await
            .unwrap();
        ReceiverStream::new(rx).map(|r| match r {
            Response::Peer(addr) => addr,
            _ => panic!("programming error"),
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    pretty_env_logger::init();

    let info_hash = Id20([0u8; 20]);
    let dht = Dht::new(&["dht.transmissionbt.com:6881"]).await.unwrap();
    let mut stream = dht.get_peers(info_hash).await;
    while let Some(peer) = stream.next().await {
        log::info!("peer found: {}", peer)
    }
    Ok(())
}
