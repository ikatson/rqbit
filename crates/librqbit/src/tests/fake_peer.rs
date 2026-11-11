//! A wire-level fake peer and small session fixtures for tests that need to control every
//! message of a connection, and the moment it is sent.
//!
//! They run a real session on loopback and talk to it with a hand-written peer.

// Helpers shared by several e2e test modules; not every module uses all of them.
#![allow(dead_code)]

use std::{
    future::Future,
    net::{Ipv4Addr, SocketAddr},
    num::NonZeroU32,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use bytes::Bytes;
use librqbit_core::Id20;
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};

use crate::{
    AddTorrent, AddTorrentOptions, ConnectionOptions, CreateTorrentOptions, ListenerOptions,
    PeerConnectionOptions, PeerPolicy, Session, SessionOptions, SessionPersistenceConfig,
    UploadSlots, create_torrent,
    limits::LimitsConfig,
    spawn_utils::BlockingSpawner,
    tests::test_util::{create_default_random_dir_with_torrents, setup_test_logging, wait_until},
    torrent_state::ManagedTorrentHandle,
};

pub(super) const PIECE_LEN: u32 = 32 * 1024;
pub(super) const BLOCK: u32 = 16 * 1024;
pub(super) const PIECES: u32 = 4;
pub(super) const FILE_LEN: usize = (PIECE_LEN * PIECES) as usize;

pub(super) async fn run(fut: impl Future<Output = anyhow::Result<()>>) -> anyhow::Result<()> {
    setup_test_logging();
    timeout(Duration::from_secs(60), fut)
        .await
        .context("test timed out")?
}

// ---------------------------------------------------------------------------------------------
// A wire-level peer
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Wire {
    KeepAlive,
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have(u32),
    Bitfield(Vec<u8>),
    Request(u32, u32, u32),
    Piece {
        index: u32,
        begin: u32,
        data: Vec<u8>,
    },
    Cancel(u32, u32, u32),
    Extended,
    Other(u8),
    // The Fast Extension (BEP 6).
    SuggestPiece(u32),
    HaveAll,
    HaveNone,
    RejectRequest(u32, u32, u32),
    AllowedFast(u32),
}

pub(super) enum Recv {
    Msg(Wire),
    Timeout,
    Closed,
}

pub(super) struct FakePeer {
    pub(super) stream: TcpStream,
    pub(super) buf: Vec<u8>,
    pub(super) local: SocketAddr,
}

pub(super) fn handshake_bytes(info_hash: Id20, peer_id: &[u8; 20]) -> [u8; 68] {
    let mut b = [0u8; 68];
    b[0] = 19;
    b[1..20].copy_from_slice(b"BitTorrent protocol");
    // The extension protocol bit (BEP 10).
    b[25] = 0x10;
    b[28..48].copy_from_slice(&info_hash.0);
    b[48..68].copy_from_slice(peer_id);
    b
}

pub(super) fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

impl FakePeer {
    pub(super) async fn connect_without_handshake(addr: SocketAddr) -> anyhow::Result<Self> {
        let stream = TcpStream::connect(addr).await.context("connect")?;
        let local = stream.local_addr()?;
        Ok(Self {
            stream,
            buf: Vec::new(),
            local,
        })
    }

    pub(super) async fn connect(
        addr: SocketAddr,
        info_hash: Id20,
        peer_id: &[u8; 20],
    ) -> anyhow::Result<Self> {
        let mut p = Self::connect_without_handshake(addr).await?;
        p.stream
            .write_all(&handshake_bytes(info_hash, peer_id))
            .await?;
        let mut reply = [0u8; 68];
        timeout(Duration::from_secs(5), p.stream.read_exact(&mut reply))
            .await
            .context("no handshake reply")?
            .context("handshake reply")?;
        anyhow::ensure!(&reply[28..48] == info_hash.0.as_slice(), "wrong info hash");
        Ok(p)
    }

    pub(super) async fn send_raw(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.stream.write_all(bytes).await?;
        Ok(())
    }

    pub(super) fn frame(id: u8, payload: &[u8]) -> Vec<u8> {
        let mut m = Vec::with_capacity(5 + payload.len());
        m.extend_from_slice(&u32::try_from(payload.len() + 1).unwrap().to_be_bytes());
        m.push(id);
        m.extend_from_slice(payload);
        m
    }

    pub(super) async fn send(&mut self, id: u8, payload: &[u8]) -> anyhow::Result<()> {
        self.send_raw(&Self::frame(id, payload)).await
    }

    pub(super) fn triple(a: u32, b: u32, c: u32) -> Vec<u8> {
        [a, b, c].iter().flat_map(|v| v.to_be_bytes()).collect()
    }

    pub(super) fn request_frame(piece: u32, begin: u32, len: u32) -> Vec<u8> {
        Self::frame(6, &Self::triple(piece, begin, len))
    }

    pub(super) async fn request(&mut self, piece: u32, begin: u32, len: u32) -> anyhow::Result<()> {
        self.send_raw(&Self::request_frame(piece, begin, len)).await
    }

    pub(super) async fn cancel(&mut self, piece: u32, begin: u32, len: u32) -> anyhow::Result<()> {
        self.send(8, &Self::triple(piece, begin, len)).await
    }

    pub(super) async fn have(&mut self, piece: u32) -> anyhow::Result<()> {
        self.send(4, &piece.to_be_bytes()).await
    }

    pub(super) async fn bitfield(&mut self, bits: &[u8]) -> anyhow::Result<()> {
        self.send(5, bits).await
    }

    pub(super) async fn interested(&mut self) -> anyhow::Result<()> {
        self.send(2, &[]).await
    }

    pub(super) async fn not_interested(&mut self) -> anyhow::Result<()> {
        self.send(3, &[]).await
    }

    pub(super) async fn choke(&mut self) -> anyhow::Result<()> {
        self.send(0, &[]).await
    }

    pub(super) async fn unchoke(&mut self) -> anyhow::Result<()> {
        self.send(1, &[]).await
    }

    pub(super) async fn keep_alive(&mut self) -> anyhow::Result<()> {
        self.send_raw(&[0; 4]).await
    }

    pub(super) async fn piece(
        &mut self,
        index: u32,
        begin: u32,
        data: &[u8],
    ) -> anyhow::Result<()> {
        let mut payload = Vec::with_capacity(8 + data.len());
        payload.extend_from_slice(&index.to_be_bytes());
        payload.extend_from_slice(&begin.to_be_bytes());
        payload.extend_from_slice(data);
        self.send(7, &payload).await
    }

    pub(super) async fn extended_handshake(&mut self, bencoded_dict: &[u8]) -> anyhow::Result<()> {
        let mut payload = vec![0u8];
        payload.extend_from_slice(bencoded_dict);
        self.send(20, &payload).await
    }

    pub(super) fn try_parse(&mut self) -> Option<Wire> {
        if self.buf.len() < 4 {
            return None;
        }
        let len = be32(&self.buf) as usize;
        if self.buf.len() < 4 + len {
            return None;
        }
        let frame: Vec<u8> = self.buf.drain(..4 + len).skip(4).collect();
        let Some((&id, p)) = frame.split_first() else {
            return Some(Wire::KeepAlive);
        };
        Some(match (id, p.len()) {
            (0, 0) => Wire::Choke,
            (1, 0) => Wire::Unchoke,
            (2, 0) => Wire::Interested,
            (3, 0) => Wire::NotInterested,
            (4, 4) => Wire::Have(be32(p)),
            (5, _) => Wire::Bitfield(p.to_vec()),
            (6, 12) => Wire::Request(be32(p), be32(&p[4..]), be32(&p[8..])),
            (7, n) if n >= 8 => Wire::Piece {
                index: be32(p),
                begin: be32(&p[4..]),
                data: p[8..].to_vec(),
            },
            (8, 12) => Wire::Cancel(be32(p), be32(&p[4..]), be32(&p[8..])),
            (20, _) => Wire::Extended,
            (0x0D, 4) => Wire::SuggestPiece(be32(p)),
            (0x0E, 0) => Wire::HaveAll,
            (0x0F, 0) => Wire::HaveNone,
            (0x10, 12) => Wire::RejectRequest(be32(p), be32(&p[4..]), be32(&p[8..])),
            (0x11, 4) => Wire::AllowedFast(be32(p)),
            (id, _) => Wire::Other(id),
        })
    }

    pub(super) async fn recv(&mut self, within: Duration) -> Recv {
        let deadline = Instant::now() + within;
        loop {
            if let Some(m) = self.try_parse() {
                return Recv::Msg(m);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let mut tmp = vec![0u8; 16 * 1024];
            match timeout(left, self.stream.read(&mut tmp)).await {
                Err(_) => return Recv::Timeout,
                Ok(Ok(0)) | Ok(Err(_)) => return Recv::Closed,
                Ok(Ok(n)) => self.buf.extend_from_slice(&tmp[..n]),
            }
        }
    }

    /// Everything that arrives within `within` (keep-alives left out), and whether the other
    /// side closed the connection meanwhile.
    pub(super) async fn collect(&mut self, within: Duration) -> (Vec<Wire>, bool) {
        let deadline = Instant::now() + within;
        let mut out = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.recv(left).await {
                Recv::Msg(Wire::KeepAlive) => {}
                Recv::Msg(m) => out.push(m),
                Recv::Timeout => return (out, false),
                Recv::Closed => return (out, true),
            }
            if left.is_zero() {
                return (out, false);
            }
        }
    }

    /// Waits for `n` pieces, returns them as (piece, begin) in arrival order together with the
    /// data. Fails when the connection ends or time runs out before.
    pub(super) async fn expect_pieces(
        &mut self,
        n: usize,
        within: Duration,
    ) -> anyhow::Result<Vec<(u32, u32, Vec<u8>)>> {
        let deadline = Instant::now() + within;
        let mut out = Vec::new();
        while out.len() < n {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.recv(left).await {
                Recv::Msg(Wire::Piece { index, begin, data }) => out.push((index, begin, data)),
                Recv::Msg(_) => {}
                Recv::Timeout => bail!("got {} of {n} pieces in {within:?}", out.len()),
                Recv::Closed => bail!("closed after {} of {n} pieces", out.len()),
            }
        }
        Ok(out)
    }

    /// Waits for `msg`, skipping whatever arrives before it.
    pub(super) async fn expect(&mut self, msg: Wire, within: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.recv(left).await {
                Recv::Msg(m) if m == msg => return Ok(()),
                Recv::Msg(_) => {}
                Recv::Timeout => bail!("no {msg:?} in {within:?}"),
                Recv::Closed => bail!("closed before {msg:?}"),
            }
        }
    }

    pub(super) async fn expect_closed(&mut self, within: Duration) -> anyhow::Result<Vec<Wire>> {
        let (msgs, closed) = self.collect(within).await;
        anyhow::ensure!(closed, "connection still open, received {msgs:?}");
        Ok(msgs)
    }
}

pub(super) fn peer_id(prefix: &[u8; 8]) -> [u8; 20] {
    let mut id = [b'x'; 20];
    id[..8].copy_from_slice(prefix);
    id
}

// ---------------------------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------------------------

pub(super) struct Fixture {
    pub(super) session: Arc<Session>,
    pub(super) handle: ManagedTorrentHandle,
    pub(super) addr: Option<SocketAddr>,
    pub(super) info_hash: Id20,
    pub(super) torrent_bytes: Bytes,
    // Concatenated content of the files of the torrent, in torrent order.
    pub(super) content: Vec<u8>,
    pub(super) _dir: TempDir,
}

impl Fixture {
    pub(super) fn block(&self, piece: u32, begin: u32, len: u32) -> &[u8] {
        let off = (piece * PIECE_LEN + begin) as usize;
        &self.content[off..off + len as usize]
    }

    pub(super) fn addr(&self) -> SocketAddr {
        self.addr.expect("fixture without a listener")
    }
}

#[derive(Default)]
pub(super) struct SeederOpts {
    pub(super) policy: Option<Arc<dyn PeerPolicy>>,
    pub(super) upload_bps: Option<u32>,
    /// Number of files of the torrent (default 1); the total size is the same.
    pub(super) files: Option<usize>,
    pub(super) upload_slots: UploadSlots,
    /// How long a peer may send nothing before the seeder gives up on it. `None`: the default.
    pub(super) read_write_timeout: Option<Duration>,
}

fn loopback_listener() -> ListenerOptions {
    ListenerOptions {
        listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
        ipv4_only: true,
        ..Default::default()
    }
}

/// A session listening on loopback that has the whole torrent.
pub(super) async fn start_seeder(opts: SeederOpts) -> anyhow::Result<Fixture> {
    start_seeder_ext(opts, SeederExt::default()).await
}

/// [`start_seeder`] with the settings of [`SeederExt`].
pub(super) async fn start_seeder_ext(opts: SeederOpts, ext: SeederExt) -> anyhow::Result<Fixture> {
    let files = opts.files.unwrap_or(1);
    let total_len = ext
        .pieces
        .map_or(FILE_LEN, |pieces| (pieces * PIECE_LEN) as usize);
    let dir = create_default_random_dir_with_torrents(
        files,
        total_len / files,
        Some("rqbit_fake_peer_seed"),
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
    let mut content = Vec::new();
    for f in 0..files {
        content.extend(std::fs::read(dir.path().join(format!("{f}.data")))?);
    }
    let torrent_bytes = torrent.as_bytes()?;
    let session = Session::new_with_opts(
        dir.path().into(),
        SessionOptions {
            dht: None,
            persistence: None,
            listen: Some(loopback_listener()),
            ipv4_only: true,
            disable_local_service_discovery: true,
            ratelimits: LimitsConfig {
                upload_bps: opts.upload_bps.and_then(NonZeroU32::new),
                download_bps: None,
            },
            peer_policy: opts.policy,
            upload_slots: opts.upload_slots,
            disable_fast_extension: ext.disable_fast_extension,
            ..Default::default()
        },
    )
    .await
    .context("error creating seeder session")?;
    let handle = session
        .add_torrent(
            AddTorrent::from_bytes(torrent_bytes.clone()),
            Some(AddTorrentOptions {
                paused: false,
                output_folder: Some(dir.path().to_str().unwrap().to_owned()),
                overwrite: true,
                peer_opts: Some(PeerConnectionOptions {
                    read_write_timeout: opts.read_write_timeout,
                    ..Default::default()
                }),
                super_seeding: ext.super_seeding,
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .context("into_handle")?;
    timeout(Duration::from_secs(20), handle.wait_until_completed())
        .await
        .context("seeder did not become a seeder")??;
    let addr = session.listen_addr();
    Ok(Fixture {
        session,
        handle,
        addr,
        info_hash: torrent.info_hash(),
        torrent_bytes,
        content,
        _dir: dir,
    })
}

#[derive(Default)]
pub(super) struct ClientOpts {
    pub(super) policy: Option<Arc<dyn PeerPolicy>>,
    pub(super) listen: bool,
    pub(super) initial_peers: Vec<SocketAddr>,
    pub(super) only_files: Option<Vec<usize>>,
    pub(super) download_bps: Option<u32>,
    /// Allow IPv6 peers (the default is IPv4 only).
    pub(super) dual_stack: bool,
    /// How long a peer may send nothing before the client gives up on it. `None`: the default.
    pub(super) read_write_timeout: Option<Duration>,
    pub(super) disable_fast_extension: bool,
    /// Keep the session's JSON persistence, with fastresume, in the client's folder.
    pub(super) persistence: bool,
    /// Download into this folder instead of a fresh temporary one, e.g. the folder of an
    /// earlier client, to pick up where it stopped.
    pub(super) dir: Option<TempDir>,
    /// Add the torrent with [`AddTorrentOptions::super_seeding`].
    pub(super) super_seeding: bool,
}

/// A client downloading the torrent of `seed` into an empty folder, or into `opts.dir`.
pub(super) async fn start_client(seed: &Fixture, opts: ClientOpts) -> anyhow::Result<Fixture> {
    let dir = match opts.dir {
        Some(dir) => dir,
        None => TempDir::with_prefix("rqbit_fake_peer_client")?,
    };
    let persistence = opts.persistence.then(|| SessionPersistenceConfig::Json {
        folder: Some(dir.path().join("session")),
    });
    let session = Session::new_with_opts(
        dir.path().into(),
        SessionOptions {
            dht: None,
            fastresume: persistence.is_some(),
            persistence,
            listen: opts.listen.then(loopback_listener),
            connect: Some(ConnectionOptions::default()),
            ipv4_only: !opts.dual_stack,
            disable_local_service_discovery: true,
            ratelimits: LimitsConfig {
                upload_bps: None,
                download_bps: opts.download_bps.and_then(NonZeroU32::new),
            },
            peer_policy: opts.policy,
            disable_fast_extension: opts.disable_fast_extension,
            ..Default::default()
        },
    )
    .await
    .context("error creating client session")?;
    let handle = session
        .add_torrent(
            AddTorrent::from_bytes(seed.torrent_bytes.clone()),
            Some(AddTorrentOptions {
                paused: false,
                only_files: opts.only_files,
                initial_peers: (!opts.initial_peers.is_empty()).then_some(opts.initial_peers),
                peer_opts: Some(PeerConnectionOptions {
                    read_write_timeout: opts.read_write_timeout,
                    ..Default::default()
                }),
                super_seeding: opts.super_seeding,
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .context("into_handle")?;
    handle.wait_until_initialized().await?;
    let addr = session.listen_addr();
    Ok(Fixture {
        session,
        handle,
        addr,
        info_hash: seed.info_hash,
        torrent_bytes: seed.torrent_bytes.clone(),
        content: Vec::new(),
        _dir: dir,
    })
}

pub(super) fn uploaded(f: &Fixture) -> u64 {
    f.handle.live().map_or(0, |l| l.get_uploaded_bytes())
}

/// A block is counted as uploaded once its write returns, which may be after the peer has read
/// it: the counter is waited for.
pub(super) async fn expect_uploaded(f: &Fixture, bytes: u64) -> anyhow::Result<()> {
    wait_until(
        || {
            let got = uploaded(f);
            anyhow::ensure!(got == bytes, "uploaded {got}, expected {bytes}");
            Ok(())
        },
        Duration::from_secs(5),
    )
    .await
}

// The harness itself: a session serves the blocks a wire-level peer asks for.
#[tokio::test(flavor = "multi_thread")]
async fn test_fake_peer_downloads_a_block_from_a_seeder() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;
        let mut peer =
            FakePeer::connect(seed.addr(), seed.info_hash, &peer_id(b"-FK0001-")).await?;
        peer.interested().await?;
        peer.request(1, BLOCK, BLOCK).await?;
        let pieces = peer.expect_pieces(1, Duration::from_secs(10)).await?;
        assert_eq!((pieces[0].0, pieces[0].1), (1, BLOCK));
        assert_eq!(pieces[0].2, seed.block(1, BLOCK, BLOCK));
        expect_uploaded(&seed, BLOCK as u64).await?;
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// The Fast Extension (BEP 6)
// ---------------------------------------------------------------------------------------------

/// Reserved handshake bytes with the extension protocol (BEP 10) and the Fast Extension (BEP 6).
pub(super) const FAST_RESERVED: [u8; 8] = [0, 0, 0, 0, 0, 0x10, 0, 0x04];

/// A handshake with the given reserved bytes.
pub(super) fn handshake_bytes_with_reserved(
    info_hash: Id20,
    peer_id: &[u8; 20],
    reserved: [u8; 8],
) -> [u8; 68] {
    let mut b = handshake_bytes(info_hash, peer_id);
    b[20..28].copy_from_slice(&reserved);
    b
}

/// Whether reserved handshake bytes carry the Fast Extension bit.
pub(super) fn reserved_supports_fast(reserved: &[u8; 8]) -> bool {
    reserved[7] & 0x04 != 0
}

impl FakePeer {
    /// Connects with the given reserved bytes. Returns the peer and the reserved bytes of the
    /// other side's handshake.
    pub(super) async fn connect_with_reserved(
        addr: SocketAddr,
        info_hash: Id20,
        peer_id: &[u8; 20],
        reserved: [u8; 8],
    ) -> anyhow::Result<(Self, [u8; 8])> {
        let mut p = Self::connect_without_handshake(addr).await?;
        p.stream
            .write_all(&handshake_bytes_with_reserved(info_hash, peer_id, reserved))
            .await?;
        let mut reply = [0u8; 68];
        timeout(Duration::from_secs(5), p.stream.read_exact(&mut reply))
            .await
            .context("no handshake reply")?
            .context("handshake reply")?;
        anyhow::ensure!(&reply[28..48] == info_hash.0.as_slice(), "wrong info hash");
        let mut theirs = [0u8; 8];
        theirs.copy_from_slice(&reply[20..28]);
        Ok((p, theirs))
    }

    /// Connects with the Fast Extension bit; fails unless the other side set it too.
    pub(super) async fn connect_fast(
        addr: SocketAddr,
        info_hash: Id20,
        peer_id: &[u8; 20],
    ) -> anyhow::Result<Self> {
        let (p, theirs) =
            Self::connect_with_reserved(addr, info_hash, peer_id, FAST_RESERVED).await?;
        anyhow::ensure!(
            reserved_supports_fast(&theirs),
            "no Fast Extension bit in {theirs:?}"
        );
        Ok(p)
    }

    /// The first message other than a keep-alive or an extended one.
    pub(super) async fn first_message(&mut self, within: Duration) -> anyhow::Result<Wire> {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.recv(left).await {
                Recv::Msg(Wire::KeepAlive | Wire::Extended) => {}
                Recv::Msg(m) => return Ok(m),
                Recv::Timeout => bail!("nothing in {within:?}"),
                Recv::Closed => bail!("closed before the first message"),
            }
        }
    }

    pub(super) async fn suggest_piece(&mut self, piece: u32) -> anyhow::Result<()> {
        self.send(0x0D, &piece.to_be_bytes()).await
    }

    pub(super) async fn have_all(&mut self) -> anyhow::Result<()> {
        self.send(0x0E, &[]).await
    }

    pub(super) async fn have_none(&mut self) -> anyhow::Result<()> {
        self.send(0x0F, &[]).await
    }

    pub(super) async fn reject(&mut self, piece: u32, begin: u32, len: u32) -> anyhow::Result<()> {
        self.send(0x10, &Self::triple(piece, begin, len)).await
    }

    pub(super) async fn allowed_fast(&mut self, piece: u32) -> anyhow::Result<()> {
        self.send(0x11, &piece.to_be_bytes()).await
    }
}

/// Seeder settings beyond [`SeederOpts`], for [`start_seeder_ext`].
#[derive(Default)]
pub(super) struct SeederExt {
    /// Pieces of the torrent. `None`: [`PIECES`].
    pub(super) pieces: Option<u32>,
    pub(super) disable_fast_extension: bool,
    /// Add the torrent with [`AddTorrentOptions::super_seeding`].
    pub(super) super_seeding: bool,
}
