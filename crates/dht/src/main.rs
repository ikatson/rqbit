use std::{
    collections::BTreeMap,
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
};
use futures::StreamExt;
use librqbit_core::peer_id::generate_peer_id;
use tokio::{
    net::UdpSocket,
    sync::mpsc::{channel, Receiver, Sender},
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
}

enum PeersOrNodes {
    Nodes(CompactNodeInfo),
    Peers(Vec<CompactPeerInfo>),
}

impl DhtState {
    fn add_searching_for_peers(&mut self, info_hash: Id20) {
        self.searching_for_peers.push(info_hash)
    }
    fn create_request(&mut self, request: Request, addr: SocketAddr) -> Message<ByteString> {
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
                if response.id != id {
                    anyhow::bail!(
                        "response id does not match: expected {:?}, received {:?}",
                        id,
                        response.id
                    )
                };
                let nodes = response
                    .nodes
                    .ok_or_else(|| anyhow::anyhow!("expected nodes for find_node requests"))?;
                self.on_found_nodes(id, nodes)
            }
            Request::GetPeers(id) => {
                if response.id != id {
                    anyhow::bail!(
                        "response id does not match: expected {:?}, received {:?}",
                        id,
                        response.id
                    )
                };
                let nodes = response
                    .nodes
                    .ok_or_else(|| anyhow::anyhow!("expected nodes for find_node requests"))?;
                // let pn = match (response.nodes, response.values) {
                //     (Some(nodes), None) => PeersOrNodes::Nodes(nodes),
                //     (None, Some(peers)) => PeersOrNodes::Peers(peers),
                //     _ => anyhow::bail!("expected nodes or values to be set in find_peers response"),
                // };
                // self.on_found_peers_or_nodes(id, pn)
            }
        };
        Ok(())
    }
    fn on_found_nodes(&mut self, target: Id20, nodes: CompactNodeInfo) {
        todo!("on_found_nodes not implemented")
    }

    fn on_found_peers_or_nodes(&mut self, target: Id20, data: PeersOrNodes) {
        todo!("on_found_nodes not implemented")
    }
}

async fn run_framer(
    socket: &UdpSocket,
    mut input_rx: Receiver<(Message<ByteString>, SocketAddr)>,
    output_tx: Sender<Message<ByteString>>,
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
                Ok(msg) => match output_tx.send(msg).await {
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

#[derive(Debug, Clone, Copy)]
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
    request_rx: Receiver<(Request, Sender<Response>)>,
    next_transaction_id: u16,
    peer_id: Id20,
}

impl DhtWorker {
    fn on_request(&self, request: Request, sender: Sender<Response>) {}

    async fn start(&mut self, bootstrap_addrs: Vec<String>) -> anyhow::Result<()> {
        let (in_tx, in_rx) = channel(1);
        let (out_tx, out_rx) = channel(1);
        let framer = run_framer(&self.socket, in_rx, out_tx);

        let bootstrap = async {
            // bootstrap
            for addr in bootstrap_addrs {
                for addr in tokio::net::lookup_host(addr).await.unwrap() {
                    // let msg = MessageKind::FindNodeRequest(FindNodeRequest {
                    //     id: self.peer_id,
                    //     target: self.peer_id,
                    // });
                    // in_tx.send((msg, addr)).await.unwrap();
                }
            }
        };
        let mut bootstrap_done = false;

        // let request_reader = async {
        //     while let Some((request, sender)) = self.request_rx.recv().await {
        //         self.on_request(request, sender)
        //     }
        // };

        // tokio::select! {
        //     _ = framer => {
        //         anyhow::bail!("framer quit")
        //     },
        //     _ = bootstrap, if !bootstrap_done => {
        //         bootstrap_done = true
        //     },
        //     _ = request_reader => {}
        // }

        todo!()
    }
}

impl Dht {
    pub async fn new(bootstrap_addrs: &[&str]) -> anyhow::Result<Self> {
        let (request_tx, request_rx) = channel(1);
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        let mut worker = DhtWorker {
            socket,
            request_rx,
            next_transaction_id: 0,
            peer_id: Id20(generate_peer_id()),
        };
        let bootstrap_addrs = bootstrap_addrs.iter().map(|s| s.to_string()).collect();
        tokio::spawn(async move { worker.start(bootstrap_addrs).await });
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
    // async fn run(self) -> anyhow::Result<Self> {
    //     let socket = UdpSocket::bind("0.0.0.0:0").await?;
    //     let (in_tx, in_rx) = channel(1);
    //     let (out_tx, out_rx) = channel(1);
    //     let framer = run_framer(socket, in_rx, out_tx);
    // }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let info_hash = Id20([0u8; 20]);
    let dht = Dht::new(&["dht.transmissionbt.com:6881"]).await.unwrap();
    let mut stream = dht.get_peers(info_hash).await;
    while let Some(peer) = stream.next().await {
        log::info!("peer found: {}", peer)
    }
    Ok(())
}
