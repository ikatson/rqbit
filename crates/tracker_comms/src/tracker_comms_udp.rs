use std::{
    collections::{hash_map::Entry, HashMap},
    net::{Ipv4Addr, SocketAddrV4},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{bail, Context};
use librqbit_core::{hash_id::Id20, spawn_utils::spawn_with_cancel};
use parking_lot::RwLock;
use rand::Rng;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error_span, trace, warn};

const ACTION_CONNECT: u32 = 0;
const ACTION_ANNOUNCE: u32 = 1;
// const ACTION_SCRAPE: u32 = 2;
// const ACTION_ERROR: u32 = 3;

pub const EVENT_NONE: u32 = 0;
pub const EVENT_COMPLETED: u32 = 1;
pub const EVENT_STARTED: u32 = 2;
pub const EVENT_STOPPED: u32 = 3;

pub type ConnectionId = u64;
const CONNECTION_ID_MAGIC: ConnectionId = 0x41727101980;

pub type TransactionId = u32;

pub fn new_transaction_id() -> TransactionId {
    rand::thread_rng().gen()
}

#[derive(Debug)]
pub struct AnnounceFields {
    pub info_hash: Id20,
    pub peer_id: Id20,
    pub downloaded: u64,
    pub left: u64,
    pub uploaded: u64,
    pub event: u32,
    pub key: u32,
    pub port: u16,
}

#[derive(Debug)]
pub enum Request {
    Connect,
    Announce(ConnectionId, AnnounceFields),
}

impl Request {
    pub fn serialize(&self, transaction_id: TransactionId, buf: &mut Vec<u8>) -> usize {
        let cur_len = buf.len();
        match self {
            Request::Connect => {
                buf.extend_from_slice(&CONNECTION_ID_MAGIC.to_be_bytes());
                buf.extend_from_slice(&ACTION_CONNECT.to_be_bytes());
                buf.extend_from_slice(&transaction_id.to_be_bytes());
            }
            Request::Announce(connection_id, fields) => {
                buf.extend_from_slice(&connection_id.to_be_bytes());
                buf.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
                buf.extend_from_slice(&transaction_id.to_be_bytes());
                buf.extend_from_slice(&fields.info_hash.0);
                buf.extend_from_slice(&fields.peer_id.0);
                buf.extend_from_slice(&fields.downloaded.to_be_bytes());
                buf.extend_from_slice(&fields.left.to_be_bytes());
                buf.extend_from_slice(&fields.uploaded.to_be_bytes());
                buf.extend_from_slice(&fields.event.to_be_bytes());
                buf.extend_from_slice(&0u32.to_be_bytes()); // ip address 0
                buf.extend_from_slice(&fields.key.to_be_bytes());
                buf.extend_from_slice(&(-1i32).to_be_bytes()); // num want -1
                buf.extend_from_slice(&fields.port.to_be_bytes());
            }
        }
        buf.len() - cur_len
    }
}

#[derive(Debug)]
pub struct AnnounceResponse {
    pub interval: u32,
    #[allow(dead_code)]
    pub leechers: u32,
    #[allow(dead_code)]
    pub seeders: u32,
    pub addrs: Vec<SocketAddrV4>,
}

#[derive(Debug)]
pub enum Response {
    Connect(ConnectionId),
    Announce(AnnounceResponse),
}

fn split_slice(s: &[u8], first_len: usize) -> Option<(&[u8], &[u8])> {
    if s.len() < first_len {
        return None;
    }
    Some(s.split_at(first_len))
}

fn s_to_arr<const T: usize>(buf: &[u8]) -> [u8; T] {
    let mut arr = [0u8; T];
    arr.copy_from_slice(buf);
    arr
}

trait ParseNum: Sized {
    fn parse_num(buf: &[u8]) -> anyhow::Result<(Self, &[u8])>;
}

macro_rules! parse_impl {
    ($ty:tt, $size:expr) => {
        impl ParseNum for $ty {
            fn parse_num(buf: &[u8]) -> anyhow::Result<($ty, &[u8])> {
                let (bytes, rest) =
                    split_slice(buf, $size).with_context(|| format!("expected {} bytes", $size))?;
                let num = $ty::from_be_bytes(s_to_arr(bytes));
                Ok((num, rest))
            }
        }
    };
}

parse_impl!(u32, 4);
parse_impl!(u64, 8);
parse_impl!(u16, 2);
parse_impl!(i32, 4);
parse_impl!(i64, 8);
parse_impl!(i16, 2);

impl Response {
    pub fn parse(buf: &[u8]) -> anyhow::Result<(TransactionId, Self)> {
        let (action, buf) = u32::parse_num(buf).context("can't parse action")?;
        let (tid, mut buf) = u32::parse_num(buf).context("can't parse transaction id")?;
        let response = match action {
            ACTION_CONNECT => {
                let (connection_id, b) =
                    u64::parse_num(buf).context("can't parse connection id")?;
                buf = b;
                Response::Connect(connection_id)
            }
            ACTION_ANNOUNCE => {
                let (interval, b) = u32::parse_num(buf).context("can't parse interval")?;
                let (leechers, b) = u32::parse_num(b).context("can't parse leechers")?;
                let (seeders, mut b) = u32::parse_num(b).context("can't parse seeders")?;
                let mut addrs = Vec::new();
                while !b.is_empty() {
                    let (ip, b2) = u32::parse_num(b)?;
                    let ip = Ipv4Addr::from(ip);
                    b = b2;

                    let (port, b2) = u16::parse_num(b)?;
                    b = b2;
                    addrs.push(SocketAddrV4::new(ip, port));
                }
                buf = b;
                Response::Announce(AnnounceResponse {
                    interval,
                    leechers,
                    seeders,
                    addrs,
                })
            }
            _ => bail!("unsupported action {action}"),
        };

        if !buf.is_empty() {
            bail!(
                "parsed {response:?} so far, but got {} remaining bytes",
                buf.len()
            );
        }

        Ok((tid, response))
    }
}

pub type TrackerAddr = (String, u16);

struct ConnectionIdMeta {
    id: ConnectionId,
    created: Instant,
}

#[derive(Default)]
struct ClientLocked {
    connections: HashMap<TrackerAddr, ConnectionIdMeta>,
    transactions: HashMap<TransactionId, tokio::sync::oneshot::Sender<Response>>,
}

struct ClientShared {
    sock: tokio::net::UdpSocket,
    locked: RwLock<ClientLocked>,
}

#[derive(Clone)]
pub struct UdpTrackerClient {
    state: Arc<ClientShared>,
}

struct TransactionIdGuard<'a> {
    tid: TransactionId,
    state: &'a ClientShared,
}

impl Drop for TransactionIdGuard<'_> {
    fn drop(&mut self) {
        let mut g = self.state.locked.write();
        g.transactions.remove(&self.tid);
    }
}

impl UdpTrackerClient {
    pub async fn new(cancel_token: CancellationToken) -> anyhow::Result<Self> {
        let sock = tokio::net::UdpSocket::bind("0.0.0.0:0")
            .await
            .context("error binding UDP for tracker")?;
        let client = Self {
            state: Arc::new(ClientShared {
                sock,
                locked: RwLock::new(Default::default()),
            }),
        };

        spawn_with_cancel(error_span!("udp_tracker"), cancel_token, {
            let client = client.clone();
            async move { client.run().await }
        });

        Ok(client)
    }

    async fn run(self) -> anyhow::Result<()> {
        let mut buf = [0u8; 16384];
        loop {
            let (len, addr) = match self.state.sock.recv_from(&mut buf).await {
                Ok(r) => r,
                Err(e) => {
                    warn!("error in UdpSocket::recv_from: {e:#}");
                    continue;
                }
            };

            let (tid, response) = match Response::parse(&buf[..len]) {
                Ok(r) => r,
                Err(e) => {
                    debug!(?addr, "error parsing UDP response: {e:#}");
                    continue;
                }
            };

            trace!(?tid, ?response, ?addr, "received");

            let t = self.state.locked.write().transactions.remove(&tid);
            match t {
                Some(tx) => match tx.send(response) {
                    Ok(_) => {}
                    Err(_) => {
                        debug!(tid, "reader dead");
                    }
                },
                None => {
                    debug!(tid, "nowhere to send response");
                }
            };
        }
    }

    async fn get_connection_id(&self, addr: &TrackerAddr) -> anyhow::Result<ConnectionId> {
        if let Some(m) = self.state.locked.read().connections.get(addr) {
            if m.created.elapsed() < Duration::from_secs(60) {
                return Ok(m.id);
            }
        }

        let response = self.request(addr, Request::Connect).await?;
        match response {
            Response::Connect(connection_id) => {
                self.state.locked.write().connections.insert(
                    addr.clone(),
                    ConnectionIdMeta {
                        id: connection_id,
                        created: Instant::now(),
                    },
                );
                Ok(connection_id)
            }
            _ => anyhow::bail!("expected connect response"),
        }
    }

    async fn request(&self, addr: &TrackerAddr, request: Request) -> anyhow::Result<Response> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let tid_g = self.reserve_transaction_id(tx)?;

        // TODO: no allocs
        let mut write_buf = Vec::new();
        request.serialize(tid_g.tid, &mut write_buf);
        self.state.sock.send_to(&write_buf, addr).await?;

        let response = tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .context("timeout connecting")?
            .context("sender dead")?;
        Ok(response)
    }

    fn reserve_transaction_id(
        &self,
        tx: tokio::sync::oneshot::Sender<Response>,
    ) -> anyhow::Result<TransactionIdGuard<'_>> {
        let mut g = self.state.locked.write();
        for _ in 0..10 {
            let t = new_transaction_id();
            match g.transactions.entry(t) {
                Entry::Occupied(_) => continue,
                Entry::Vacant(vac) => {
                    vac.insert(tx);
                    return Ok(TransactionIdGuard {
                        tid: t,
                        state: &self.state,
                    });
                }
            }
        }
        bail!("cant generate transaction id")
    }

    pub async fn announce(
        &self,
        tracker: &TrackerAddr,
        fields: AnnounceFields,
    ) -> anyhow::Result<AnnounceResponse> {
        let connection_id = self.get_connection_id(tracker).await?;
        let request = Request::Announce(connection_id, fields);
        let response = self.request(tracker, request).await?;
        match response {
            Response::Announce(r) => Ok(r),
            other => bail!("unexpected response {other:?}, expected announce"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Write, str::FromStr};

    use librqbit_core::{hash_id::Id20, peer_id::generate_peer_id};

    use crate::tracker_comms_udp::{
        new_transaction_id, AnnounceFields, Request, Response, EVENT_NONE,
    };

    #[test]
    fn test_parse_announce() {
        let b = include_bytes!("../resources/test/udp-tracker-announce-response.bin");
        let (tid, response) = Response::parse(b).unwrap();
        dbg!(tid, response);
    }

    #[ignore]
    #[tokio::test]
    async fn test_announce() {
        let sock = tokio::net::UdpSocket::bind("0.0.0.0:0").await.unwrap();
        sock.connect("opentor.net:6969").await.unwrap();

        let tid = new_transaction_id();
        let mut write_buf = Vec::new();
        let mut read_buf = vec![0u8; 4096];

        Request::Connect.serialize(tid, &mut write_buf);

        sock.send(&write_buf).await.unwrap();

        let size = sock.recv(&mut read_buf).await.unwrap();

        let (rtid, response) = Response::parse(&read_buf[..size]).unwrap();
        assert_eq!(tid, rtid);
        let connection_id = match response {
            Response::Connect(connection_id) => {
                dbg!(connection_id)
            }
            other => panic!("unexpected response {:?}", other),
        };

        let hash = Id20::from_str("775459190aa65566591634203f8d9f17d341f969").unwrap();

        let tid = new_transaction_id();
        let request = Request::Announce(
            connection_id,
            AnnounceFields {
                info_hash: hash,
                peer_id: generate_peer_id(),
                downloaded: 0,
                left: 0,
                uploaded: 0,
                event: EVENT_NONE,
                key: 0, // whatever that is?
                port: 24563,
            },
        );
        write_buf.clear();
        let size = request.serialize(tid, &mut write_buf);

        sock.send(&write_buf[..size]).await.unwrap();
        let size = sock.recv(&mut read_buf).await.unwrap();

        {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open("/tmp/proto.bin")
                .unwrap();
            f.write_all(&read_buf[..size]).unwrap();
        }

        dbg!(&read_buf[..size]);
        let (rtid, response) = Response::parse(&read_buf[..size]).unwrap();
        assert_eq!(tid, rtid);
        match response {
            Response::Announce(r) => {
                dbg!(r);
            }
            other => panic!("unexpected response {:?}", other),
        }
    }
}
