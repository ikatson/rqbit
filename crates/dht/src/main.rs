use std::{collections::HashMap, net::SocketAddrV4};

use crate::bprotocol::MessageKind;
use bencode::ByteString;
use librqbit_core::peer_id::generate_peer_id;
use log::debug;
use parking_lot::Mutex;

use crate::bprotocol::Message;

mod bprotocol;

struct SocketManager {
    socket: tokio::net::UdpSocket,
    rx: tokio::sync::mpsc::Receiver<(
        SocketAddrV4,
        MessageKind<ByteString>,
        tokio::sync::oneshot::Sender<Message<ByteString>>,
    )>,
}

impl SocketManager {
    pub async fn spawn() -> anyhow::Result<SocketManagerHandle> {
        let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let mgr = SocketManager { socket, rx };
        tokio::spawn(mgr.run());
        Ok(SocketManagerHandle { tx })
    }
    pub async fn run(self) -> anyhow::Result<()> {
        let Self { socket, mut rx } = self;

        let mut transaction_id = 0u16;
        let mut next_transaction_id = move || {
            let next = transaction_id;
            transaction_id = next + 1;
            next
        };

        let outstanding = Mutex::new(HashMap::<
            u16,
            tokio::sync::oneshot::Sender<Message<ByteString>>,
        >::new());

        let writer = async {
            let mut buf = Vec::new();
            while let Some((addr, msg, tx)) = rx.recv().await {
                let transaction_id = next_transaction_id();
                let transaction_id_buf =
                    [(transaction_id >> 8) as u8, (transaction_id & 0xff) as u8];
                buf.clear();
                bprotocol::serialize_message(
                    &mut buf,
                    // this is bad, allocates
                    ByteString::from(transaction_id_buf.as_ref()),
                    None,
                    None,
                    msg,
                )
                .unwrap();

                debug!("inserting transaction id {}", transaction_id);
                assert!(outstanding.lock().insert(transaction_id, tx).is_none());
                debug!("sending msg to {}", addr);
                socket.send_to(&buf, addr).await.unwrap();
            }
        };

        let reader = async {
            let mut buf = vec![0u8; 16384];
            while let Ok(size) = socket.recv(&mut buf).await {
                debug!("received {}", size);
                let msg = match bprotocol::deserialize_message::<ByteString>(&buf[..size]) {
                    Ok(msg) => msg,
                    // todo handle errors
                    Err(e) => panic!("{}", e),
                };
                assert!(msg.transaction_id.len() == 2);
                let b0 = msg.transaction_id[0];
                let b1 = msg.transaction_id[1];
                let tid = ((b0 as u16) << 8) + b1 as u16;
                let tx = outstanding.lock().remove(&tid).unwrap();
                debug!("sending oneshot result, tid {}", tid);
                tx.send(msg).unwrap();
            }
        };

        tokio::select! {
            _ = writer => {},
            _ = reader => {}
        }

        Ok(())
    }
}

#[derive(Clone)]
struct SocketManagerHandle {
    tx: tokio::sync::mpsc::Sender<(
        SocketAddrV4,
        MessageKind<ByteString>,
        tokio::sync::oneshot::Sender<Message<ByteString>>,
    )>,
}

impl SocketManagerHandle {
    async fn request(
        &self,
        addr: SocketAddrV4,
        kind: MessageKind<ByteString>,
    ) -> anyhow::Result<bprotocol::Message<ByteString>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.tx.send((addr, kind, tx)).await?;
        let msg = rx.await?;
        Ok(msg)
    }
}

#[tokio::main]
async fn main() {
    std::env::set_var("RUST_LOG", "trace");
    pretty_env_logger::init();

    let mgr = SocketManager::spawn().await.unwrap();

    let peer_id = bprotocol::Id20(generate_peer_id());
    for first_addr in tokio::net::lookup_host("dht.transmissionbt.com:6881")
        .await
        .unwrap()
        .filter_map(|a| match a {
            std::net::SocketAddr::V4(v4) => Some(v4),
            std::net::SocketAddr::V6(_) => None,
        })
        .skip(1)
    {
        let msg = bprotocol::MessageKind::FindNodeRequest(bprotocol::FindNodeRequest {
            id: peer_id,
            target: peer_id,
        });

        dbg!(mgr.request(first_addr, msg).await.unwrap());
    }
}
