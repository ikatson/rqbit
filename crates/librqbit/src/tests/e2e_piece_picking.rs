//! Which pieces a downloading session asks a peer for: rarest first, sequential, first and last
//! pieces first, and a change of mode while the torrent is live.
//!
//! Hand-written wire-level peers connect to a real session on loopback, so the order of the
//! session's requests is observed directly. Pieces are one block long: one request per piece.

use std::{
    future::Future,
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

use anyhow::{Context, bail};
use bytes::Bytes;
use librqbit_core::Id20;
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{Instant, timeout},
};

use crate::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, ListenerOptions, PiecePicking, Session,
    SessionOptions, create_torrent,
    spawn_utils::BlockingSpawner,
    tests::test_util::{create_default_random_dir_with_torrents, setup_test_logging},
    torrent_state::ManagedTorrentHandle,
};

const PIECE_LEN: u32 = 16 * 1024;

async fn run(fut: impl Future<Output = anyhow::Result<()>>) -> anyhow::Result<()> {
    setup_test_logging();
    timeout(Duration::from_secs(60), fut)
        .await
        .context("test timed out")?
}

struct Torrent {
    info_hash: Id20,
    bytes: Bytes,
    // The single file's content.
    content: Vec<u8>,
    _dir: TempDir,
}

async fn make_torrent(pieces: u32) -> anyhow::Result<Torrent> {
    let dir = create_default_random_dir_with_torrents(
        1,
        (PIECE_LEN * pieces) as usize,
        Some("rqbit_piece_picking_src"),
    );
    let torrent = create_torrent(
        dir.path(),
        CreateTorrentOptions {
            name: None,
            piece_length: Some(PIECE_LEN),
            ..Default::default()
        },
        &BlockingSpawner::new(1),
    )
    .await?;
    Ok(Torrent {
        info_hash: torrent.info_hash(),
        bytes: torrent.as_bytes()?,
        content: std::fs::read(dir.path().join("0.data"))?,
        _dir: dir,
    })
}

struct Client {
    session: std::sync::Arc<Session>,
    handle: ManagedTorrentHandle,
    addr: SocketAddr,
    _dir: TempDir,
}

/// A session downloading `torrent`, listening for peers. `have_pieces` of the file's leading
/// pieces are already on disk.
async fn start_client(
    torrent: &Torrent,
    piece_picking: PiecePicking,
    have_pieces: u32,
) -> anyhow::Result<Client> {
    let dir = TempDir::with_prefix("rqbit_piece_picking_client")?;
    if have_pieces > 0 {
        let mut data = vec![0u8; torrent.content.len()];
        let had = (have_pieces * PIECE_LEN) as usize;
        data[..had].copy_from_slice(&torrent.content[..had]);
        std::fs::write(dir.path().join("0.data"), data)?;
    }
    let session = Session::new_with_opts(
        dir.path().into(),
        SessionOptions {
            dht: None,
            persistence: None,
            listen: Some(ListenerOptions {
                listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
                ipv4_only: true,
                ..Default::default()
            }),
            ipv4_only: true,
            disable_local_service_discovery: true,
            ..Default::default()
        },
    )
    .await
    .context("error creating client session")?;
    let handle = session
        .add_torrent(
            AddTorrent::from_bytes(torrent.bytes.clone()),
            Some(AddTorrentOptions {
                output_folder: Some(dir.path().to_str().context("temp dir path")?.to_owned()),
                overwrite: true,
                piece_picking,
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .context("into_handle")?;
    handle.wait_until_initialized().await?;
    let addr = session.listen_addr().context("client is not listening")?;
    Ok(Client {
        session,
        handle,
        addr,
        _dir: dir,
    })
}

struct FakePeer {
    stream: TcpStream,
    buf: Vec<u8>,
}

enum Msg {
    Interested,
    Request(u32),
    Other,
}

impl FakePeer {
    async fn connect(addr: SocketAddr, info_hash: Id20, id: u8) -> anyhow::Result<Self> {
        let mut stream = TcpStream::connect(addr).await.context("connect")?;
        let mut hs = [0u8; 68];
        hs[0] = 19;
        hs[1..20].copy_from_slice(b"BitTorrent protocol");
        hs[28..48].copy_from_slice(&info_hash.0);
        hs[48..68].copy_from_slice(&[b'a' + id; 20]);
        stream.write_all(&hs).await?;
        let mut reply = [0u8; 68];
        timeout(Duration::from_secs(5), stream.read_exact(&mut reply))
            .await
            .context("no handshake reply")??;
        Ok(Self {
            stream,
            buf: Vec::new(),
        })
    }

    async fn send(&mut self, id: u8, payload: &[u8]) -> anyhow::Result<()> {
        let mut m = Vec::with_capacity(5 + payload.len());
        m.extend_from_slice(&u32::try_from(payload.len() + 1)?.to_be_bytes());
        m.push(id);
        m.extend_from_slice(payload);
        self.stream.write_all(&m).await?;
        Ok(())
    }

    async fn bitfield(&mut self, pieces: u32, has: impl Fn(u32) -> bool) -> anyhow::Result<()> {
        let mut bits = vec![0u8; pieces.div_ceil(8) as usize];
        for p in (0..pieces).filter(|p| has(*p)) {
            bits[(p / 8) as usize] |= 0x80 >> (p % 8);
        }
        self.send(5, &bits).await
    }

    async fn recv(&mut self, deadline: Instant) -> anyhow::Result<Msg> {
        loop {
            if self.buf.len() >= 4 {
                let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]])
                    as usize;
                if self.buf.len() >= 4 + len {
                    let frame: Vec<u8> = self.buf.drain(..4 + len).skip(4).collect();
                    return Ok(match frame.split_first() {
                        Some((2, [])) => Msg::Interested,
                        Some((6, p)) if p.len() == 12 => {
                            Msg::Request(u32::from_be_bytes([p[0], p[1], p[2], p[3]]))
                        }
                        _ => Msg::Other,
                    });
                }
            }
            let mut tmp = [0u8; 4096];
            let n = timeout(
                deadline.saturating_duration_since(Instant::now()),
                self.stream.read(&mut tmp),
            )
            .await
            .context("timed out waiting for a message")??;
            if n == 0 {
                bail!("connection closed");
            }
            self.buf.extend_from_slice(&tmp[..n]);
        }
    }

    async fn wait_interested(&mut self) -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !matches!(self.recv(deadline).await?, Msg::Interested) {}
        Ok(())
    }

    /// The pieces of the first `n` requests, in order. Nothing is served.
    async fn first_requests(&mut self, n: usize) -> anyhow::Result<Vec<u32>> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut out = Vec::new();
        while out.len() < n {
            if let Msg::Request(piece) = self.recv(deadline).await? {
                out.push(piece);
            }
        }
        Ok(out)
    }
}

/// A seed that unchokes right away.
async fn connect_seed(client: &Client, torrent: &Torrent, pieces: u32) -> anyhow::Result<FakePeer> {
    let mut seed = FakePeer::connect(client.addr, torrent.info_hash, 0).await?;
    seed.bitfield(pieces, |_| true).await?;
    seed.send(1, &[]).await?;
    Ok(seed)
}

#[tokio::test(flavor = "multi_thread")]
async fn test_the_piece_fewest_peers_have_is_requested_first() -> anyhow::Result<()> {
    run(async {
        const PIECES: u32 = 32;
        const RARE: u32 = 20;
        let torrent = make_torrent(PIECES).await?;
        // Past the first few pieces, which are picked at random.
        let client = start_client(&torrent, PiecePicking::default(), 8).await?;

        // A choking peer that has everything but RARE: only it makes the other pieces common.
        let mut common = FakePeer::connect(client.addr, torrent.info_hash, 1).await?;
        common.bitfield(PIECES, |p| p != RARE).await?;
        common.wait_interested().await?;

        let mut seed = connect_seed(&client, &torrent, PIECES).await?;
        assert_eq!(seed.first_requests(1).await?, vec![RARE]);
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_first_and_last_pieces_are_requested_first() -> anyhow::Result<()> {
    run(async {
        // 1% of 300 pieces is 3 at each end.
        const PIECES: u32 = 300;
        let torrent = make_torrent(PIECES).await?;
        let picking = PiecePicking {
            first_last_pieces_first: true,
            ..Default::default()
        };
        let client = start_client(&torrent, picking, 0).await?;

        let mut seed = connect_seed(&client, &torrent, PIECES).await?;
        assert_eq!(seed.first_requests(6).await?, vec![0, 1, 2, 297, 298, 299]);
        // And then the rest, not in order.
        let rest = seed.first_requests(16).await?;
        assert_ne!(rest, (3..19).collect::<Vec<_>>());
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_switching_a_live_torrent_to_sequential() -> anyhow::Result<()> {
    run(async {
        const PIECES: u32 = 64;
        let torrent = make_torrent(PIECES).await?;
        let client = start_client(&torrent, PiecePicking::default(), 0).await?;
        assert!(client.handle.live().is_some(), "torrent is not live");

        let sequential = PiecePicking {
            sequential: true,
            ..Default::default()
        };
        client
            .session
            .set_piece_picking(&client.handle, sequential)
            .await;
        assert_eq!(client.handle.piece_picking(), sequential);

        let mut seed = connect_seed(&client, &torrent, PIECES).await?;
        assert_eq!(seed.first_requests(16).await?, (0..16).collect::<Vec<_>>());
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_piece_picking_survives_pause_and_resume() -> anyhow::Result<()> {
    run(async {
        const PIECES: u32 = 64;
        let torrent = make_torrent(PIECES).await?;
        let sequential = PiecePicking {
            sequential: true,
            ..Default::default()
        };
        let client = start_client(&torrent, sequential, 0).await?;
        client.session.pause(&client.handle).await?;
        client.session.unpause(&client.handle).await?;

        let mut seed = connect_seed(&client, &torrent, PIECES).await?;
        assert_eq!(seed.first_requests(16).await?, (0..16).collect::<Vec<_>>());
        Ok(())
    })
    .await
}
