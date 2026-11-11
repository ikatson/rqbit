//! BitTorrent v2 (BEP 52) torrents between real sessions on loopback.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;
use bytes::Bytes;
use librqbit_core::{Id20, metainfo_v2::TorrentVersion, torrent_metainfo::torrent_from_bytes};
use peer_binary_protocol::Handshake;
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

use crate::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ConnectionOptions, ListenerOptions, Session,
    SessionOptions, SessionPersistenceConfig,
    api::TorrentIdOrHash,
    tests::{
        fake_peer::{FakePeer, Recv, Wire, handshake_bytes, peer_id, run},
        v2_torrent::V2Files,
    },
    torrent_state::ManagedTorrentHandle,
};

pub(super) const PIECE_LEN: u32 = 32 * 1024;
const BLOCK: u32 = 16 * 1024;

/// Files covering the layout cases: a file of several pieces with a short
/// last one, an exact multiple of the piece length, a file smaller than a
/// piece, an empty file, nested directories and non-ASCII names.
pub(super) fn sample_files(seed: u64) -> V2Files {
    let pl = PIECE_LEN as usize;
    V2Files::random(
        &[
            ("big.bin", 3 * pl + 5000),
            ("dir/exact.bin", 2 * pl),
            ("dir/small.bin", 1000),
            ("empty.bin", 0),
            ("ünï/名前.bin", pl + 1),
        ],
        seed,
    )
}

#[derive(Default)]
pub(super) struct SessionSetup {
    pub(super) listen: bool,
    pub(super) persistence: Option<PathBuf>,
}

pub(super) async fn new_session(dir: &Path, setup: SessionSetup) -> anyhow::Result<Arc<Session>> {
    Session::new_with_opts(
        dir.into(),
        SessionOptions {
            dht: None,
            persistence: setup
                .persistence
                .map(|folder| SessionPersistenceConfig::Json {
                    folder: Some(folder),
                }),
            fastresume: true,
            listen: setup.listen.then(|| ListenerOptions {
                listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
                ipv4_only: true,
                ..Default::default()
            }),
            connect: Some(ConnectionOptions::default()),
            ipv4_only: true,
            disable_local_service_discovery: true,
            ..Default::default()
        },
    )
    .await
}

pub(super) async fn add(
    session: &Arc<Session>,
    torrent: Bytes,
    output_folder: &Path,
    peers: Vec<SocketAddr>,
) -> anyhow::Result<ManagedTorrentHandle> {
    session
        .add_torrent(
            AddTorrent::from_bytes(torrent),
            Some(AddTorrentOptions {
                output_folder: Some(output_folder.to_str().context("utf-8 path")?.to_owned()),
                overwrite: true,
                initial_peers: (!peers.is_empty()).then_some(peers),
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .context("into_handle")
}

pub(super) async fn completed(
    handle: &ManagedTorrentHandle,
    within: Duration,
) -> anyhow::Result<()> {
    timeout(within, handle.wait_until_completed())
        .await
        .context("did not complete in time")?
}

fn have(handle: &ManagedTorrentHandle, piece: u32) -> bool {
    handle
        .live()
        .and_then(|l| {
            l.lock_read("test")
                .get_chunks()
                .ok()
                .map(|c| c.get_have_pieces().as_slice()[piece as usize])
        })
        .unwrap_or(false)
}

/// A v2-only torrent: a seeder verifies its files with merkle hashes, a client
/// downloads them, seeds them on to a third session, and comes back complete
/// from its fastresume state after a restart, with no peer to download from.
#[tokio::test(flavor = "multi_thread")]
async fn test_e2e_v2_only_download_seed_restart() -> anyhow::Result<()> {
    run(async {
        let files = sample_files(1);
        let torrent = Bytes::from(files.torrent("v2", PIECE_LEN, false).to_bytes());
        let parsed = torrent_from_bytes(&torrent)?;
        let info = parsed.validate_with_piece_layers()?;
        assert_eq!(info.version(), TorrentVersion::V2);
        let info_hash = parsed.info_hash;
        let v2_hash = parsed.info_hash_v2.context("no v2 hash")?;
        assert_eq!(info_hash.0, v2_hash.0[..20]);
        assert_eq!(info.lengths().total_length(), files.total_len());

        let seed_dir = TempDir::with_prefix("rqbit_v2_seed")?;
        files.write_to(seed_dir.path());
        let seeder = new_session(
            seed_dir.path(),
            SessionSetup {
                listen: true,
                ..Default::default()
            },
        )
        .await?;
        let seeder_handle = add(&seeder, torrent.clone(), seed_dir.path(), vec![]).await?;
        completed(&seeder_handle, Duration::from_secs(20))
            .await
            .context("the seeder's own files did not verify")?;
        let seeder_addr = seeder.listen_addr().context("seeder listen addr")?;

        let client_dir = TempDir::with_prefix("rqbit_v2_client")?;
        let out = client_dir.path().join("out");
        let persistence = client_dir.path().join("session");
        let client = new_session(
            &out,
            SessionSetup {
                listen: true,
                persistence: Some(persistence.clone()),
                ..Default::default()
            },
        )
        .await?;
        let handle = add(&client, torrent.clone(), &out, vec![seeder_addr]).await?;
        assert_eq!(handle.info_hash(), info_hash);
        completed(&handle, Duration::from_secs(60)).await?;
        files.assert_equal_to(&out);
        let stats = handle.stats();
        assert_eq!(stats.total_bytes, files.total_len());
        assert_eq!(stats.progress_bytes, files.total_len());

        // Pausing and resuming a finished v2 torrent keeps it finished.
        client.pause(&handle).await?;
        client.unpause(&handle).await?;
        completed(&handle, Duration::from_secs(20)).await?;

        // The client seeds: a third session that only knows the client gets
        // everything from it after the original seeder is gone.
        seeder.stop().await;
        drop(seeder_handle);
        drop(seeder);
        let client_addr = client.listen_addr().context("client listen addr")?;
        let third_dir = TempDir::with_prefix("rqbit_v2_third")?;
        let third = new_session(third_dir.path(), SessionSetup::default()).await?;
        let third_handle =
            add(&third, torrent.clone(), third_dir.path(), vec![client_addr]).await?;
        completed(&third_handle, Duration::from_secs(60)).await?;
        files.assert_equal_to(third_dir.path());
        third.stop().await;

        // Restart: the torrent comes back from the session file, its piece
        // bitfield from the fastresume file.
        client.stop().await;
        drop(handle);
        drop(client);
        let bitv = std::fs::read_dir(&persistence)?
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".bitv"));
        assert!(bitv, "no fastresume bitfield in {persistence:?}");

        let client = new_session(
            &out,
            SessionSetup {
                listen: false,
                persistence: Some(persistence.clone()),
                ..Default::default()
            },
        )
        .await?;
        let handle = client
            .get(TorrentIdOrHash::Hash(info_hash))
            .context("torrent not restored")?;
        let restored_v2 = handle.metadata.load().as_ref().map(|m| m.info.version());
        assert_eq!(restored_v2, Some(TorrentVersion::V2));
        completed(&handle, Duration::from_secs(20)).await?;
        assert_eq!(handle.stats().progress_bytes, files.total_len());
        files.assert_equal_to(&out);
        client.stop().await;
        Ok(())
    })
    .await
}

/// A hybrid torrent between two sessions: both hashes are checked for every
/// piece, the padding of the v1 layout is never written, and progress counts
/// the bytes of real files only.
#[tokio::test(flavor = "multi_thread")]
async fn test_e2e_hybrid_download() -> anyhow::Result<()> {
    run(async {
        let files = sample_files(2);
        let torrent = Bytes::from(files.torrent("hybrid", PIECE_LEN, true).to_bytes());
        let parsed = torrent_from_bytes(&torrent)?;
        assert_eq!(
            parsed.validate_with_piece_layers()?.version(),
            TorrentVersion::Hybrid
        );
        assert!(parsed.info_hash_v2.is_some());

        let seed_dir = TempDir::with_prefix("rqbit_hybrid_seed")?;
        files.write_to(seed_dir.path());
        let seeder = new_session(
            seed_dir.path(),
            SessionSetup {
                listen: true,
                ..Default::default()
            },
        )
        .await?;
        let seeder_handle = add(&seeder, torrent.clone(), seed_dir.path(), vec![]).await?;
        completed(&seeder_handle, Duration::from_secs(20)).await?;

        let client_dir = TempDir::with_prefix("rqbit_hybrid_client")?;
        let client = new_session(client_dir.path(), SessionSetup::default()).await?;
        let handle = add(
            &client,
            torrent.clone(),
            client_dir.path(),
            vec![seeder.listen_addr().context("listen addr")?],
        )
        .await?;
        assert_eq!(handle.info_hash(), parsed.info_hash);
        completed(&handle, Duration::from_secs(60)).await?;
        files.assert_equal_to(client_dir.path());
        assert!(!client_dir.path().join(".pad").exists());
        let stats = handle.stats();
        assert_eq!(stats.total_bytes, files.total_len());
        assert_eq!(stats.progress_bytes, files.total_len());
        client.stop().await;
        seeder.stop().await;
        Ok(())
    })
    .await
}

/// The handshake of a wire-level fake peer for `info_hash`, with the BEP 52
/// "supports v2" bit set if `v2`.
pub(super) fn fake_handshake(info_hash: Id20, v2: bool) -> [u8; 68] {
    let mut h = handshake_bytes(info_hash, &peer_id(b"-FK0002-"));
    if v2 {
        h[27] |= 0x10;
    }
    h
}

/// Opens a plaintext connection to `addr` and exchanges handshakes carrying
/// `info_hash`. Returns the peer and the handshake it got back.
pub(super) async fn connect_with_hash(
    addr: SocketAddr,
    info_hash: Id20,
    v2: bool,
) -> anyhow::Result<(FakePeer, Handshake)> {
    let mut peer = FakePeer::connect_without_handshake(addr).await?;
    peer.send_raw(&fake_handshake(info_hash, v2)).await?;
    let reply = read_handshake(&mut peer.stream).await?;
    Ok((peer, reply))
}

async fn read_handshake(stream: &mut TcpStream) -> anyhow::Result<Handshake> {
    let mut buf = [0u8; 68];
    timeout(Duration::from_secs(10), stream.read_exact(&mut buf))
        .await
        .context("no handshake")?
        .context("reading the handshake")?;
    Ok(Handshake::deserialize(&buf).context("bad handshake")?.0)
}

/// A hybrid torrent is one torrent in two swarms: a seeder answers peers that
/// connect with its v1 info hash and peers that connect with its truncated v2
/// one, each with the hash it used, and serves both the same v1 pieces
/// (padding included). The session finds the torrent by either hash, and
/// adding it again resolves to the same torrent.
#[tokio::test(flavor = "multi_thread")]
async fn test_e2e_hybrid_v1_and_v2_peers() -> anyhow::Result<()> {
    run(async {
        let files = sample_files(4);
        let torrent = Bytes::from(files.torrent("hybrid", PIECE_LEN, true).to_bytes());
        let parsed = torrent_from_bytes(&torrent)?;
        let v1 = parsed.info_hash;
        let v2 = parsed
            .info_hash_v2
            .context("no v2 hash")?
            .truncate_for_dht();
        assert_ne!(v1, v2);

        let seed_dir = TempDir::with_prefix("rqbit_hybrid_swarms")?;
        files.write_to(seed_dir.path());
        let seeder = new_session(
            seed_dir.path(),
            SessionSetup {
                listen: true,
                ..Default::default()
            },
        )
        .await?;
        let handle = add(&seeder, torrent.clone(), seed_dir.path(), vec![]).await?;
        completed(&handle, Duration::from_secs(20)).await?;
        assert_eq!(handle.info_hash(), v1);
        assert_eq!(handle.info_hash_v2(), parsed.info_hash_v2);
        for hash in [v1, v2] {
            let found = seeder.get(TorrentIdOrHash::Hash(hash)).map(|h| h.id());
            assert_eq!(found, Some(handle.id()), "not found by {hash:?}");
        }
        let again = seeder
            .add_torrent(
                AddTorrent::from_bytes(torrent.clone()),
                Some(AddTorrentOptions {
                    output_folder: Some(seed_dir.path().to_str().context("utf-8")?.to_owned()),
                    ..Default::default()
                }),
            )
            .await?;
        assert!(
            matches!(again, AddTorrentResponse::AlreadyManaged(id, _) if id == handle.id()),
            "added twice"
        );

        // The first piece of `big.bin`, and its last one, which the v1 layout
        // pads with zeros up to the piece length.
        let big = &files.files[0].1;
        let pl = PIECE_LEN as usize;
        let mut padded = big[3 * pl..].to_vec();
        padded.resize(BLOCK as usize, 0);
        let addr = seeder.listen_addr().context("listen addr")?;
        for (hash, v2_bit) in [(v1, false), (v1, true), (v2, true)] {
            let (mut peer, reply) = connect_with_hash(addr, hash, v2_bit).await?;
            assert_eq!(reply.info_hash, hash, "answered with another hash");
            assert!(reply.supports_v2(), "no v2 bit in the reply");
            peer.interested().await?;
            peer.request(0, 0, BLOCK).await?;
            peer.request(3, 0, BLOCK).await?;
            let mut got = peer.expect_pieces(2, Duration::from_secs(10)).await?;
            got.sort_by_key(|(index, _, _)| *index);
            assert_eq!(got[0].2, big[..BLOCK as usize], "piece 0 via {hash:?}");
            assert_eq!(got[1].2, padded, "piece 3 via {hash:?}");
        }
        seeder.stop().await;
        Ok(())
    })
    .await
}

/// A peer that knows a hybrid torrent only by its v2 info hash closes
/// connections that carry the v1 one: we connect to it again with the
/// truncated v2 hash, and the connection then works.
#[tokio::test(flavor = "multi_thread")]
async fn test_e2e_hybrid_outgoing_falls_back_to_v2_hash() -> anyhow::Result<()> {
    run(async {
        let files = sample_files(5);
        let torrent = Bytes::from(files.torrent("hybrid", PIECE_LEN, true).to_bytes());
        let parsed = torrent_from_bytes(&torrent)?;
        let v1 = parsed.info_hash;
        let v2 = parsed
            .info_hash_v2
            .context("no v2 hash")?
            .truncate_for_dht();
        let num_pieces = parsed
            .validate_with_piece_layers()?
            .lengths()
            .total_pieces();

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let client_dir = TempDir::with_prefix("rqbit_hybrid_fallback")?;
        let client = new_session(client_dir.path(), SessionSetup::default()).await?;
        let _handle = add(
            &client,
            torrent,
            client_dir.path(),
            vec![listener.local_addr()?],
        )
        .await?;

        let accept = || async {
            timeout(Duration::from_secs(10), listener.accept())
                .await
                .context("no connection")?
                .context("accept")
        };
        let (mut conn, _) = accept().await?;
        let first = read_handshake(&mut conn).await?;
        assert_eq!(
            first.info_hash, v1,
            "the first attempt is not with the v1 hash"
        );
        assert!(first.supports_v2(), "no v2 bit");
        drop(conn);

        let (mut conn, _) = accept().await?;
        let second = read_handshake(&mut conn).await?;
        assert_eq!(second.info_hash, v2, "no retry with the v2 hash");
        assert!(second.supports_v2(), "no v2 bit");
        conn.write_all(&fake_handshake(v2, true)).await?;
        let local = conn.local_addr()?;
        let mut peer = FakePeer {
            stream: conn,
            buf: Vec::new(),
            local,
        };
        let mut bits = vec![0u8; num_pieces.div_ceil(8) as usize];
        for i in 0..num_pieces {
            bits[(i / 8) as usize] |= 0x80 >> (i % 8);
        }
        peer.bitfield(&bits).await?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match peer
                .recv(deadline.saturating_duration_since(Instant::now()))
                .await
            {
                Recv::Msg(Wire::Interested) => break,
                Recv::Msg(_) => {}
                Recv::Timeout => anyhow::bail!("no Interested on the connection with the v2 hash"),
                Recv::Closed => anyhow::bail!("the connection with the v2 hash was closed"),
            }
        }
        client.stop().await;
        Ok(())
    })
    .await
}

/// A peer that corrupts a block of a v2 torrent: the merkle check rejects the
/// piece, the peer is dropped, and the piece is downloaded again from an
/// honest seeder.
#[tokio::test(flavor = "multi_thread")]
async fn test_e2e_v2_corrupted_block_is_redownloaded() -> anyhow::Result<()> {
    run(async {
        let files = sample_files(3);
        let torrent = Bytes::from(files.torrent("v2", PIECE_LEN, false).to_bytes());
        let info_hash: Id20 = torrent_from_bytes(&torrent)?.info_hash;
        let pieces = files.v2_pieces(PIECE_LEN);
        let num_pieces = u32::try_from(pieces.len())?;

        let client_dir = TempDir::with_prefix("rqbit_v2_corrupt_client")?;
        let client = new_session(
            client_dir.path(),
            SessionSetup {
                listen: true,
                ..Default::default()
            },
        )
        .await?;
        let handle = add(&client, torrent.clone(), client_dir.path(), vec![]).await?;
        handle.wait_until_initialized().await?;

        // The fake peer has everything and serves every request, with the
        // first block it is asked for flipped.
        let mut peer = FakePeer::connect(
            client.listen_addr().context("listen addr")?,
            info_hash,
            &peer_id(b"-FK0001-"),
        )
        .await?;
        let mut bits = vec![0u8; num_pieces.div_ceil(8) as usize];
        for i in 0..num_pieces {
            bits[(i / 8) as usize] |= 0x80 >> (i % 8);
        }
        peer.bitfield(&bits).await?;
        peer.unchoke().await?;
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut corrupted = None;
        loop {
            match peer
                .recv(deadline.saturating_duration_since(Instant::now()))
                .await
            {
                Recv::Msg(Wire::Request(index, begin, len)) => {
                    let piece = pieces.get(index as usize).context("request past the end")?;
                    let mut data = piece
                        .get(begin as usize..(begin + len) as usize)
                        .with_context(|| format!("request {index}/{begin}/{len} past the piece"))?
                        .to_vec();
                    if corrupted.is_none() {
                        data[0] ^= 1;
                        corrupted = Some(index);
                    }
                    peer.piece(index, begin, &data).await?;
                }
                Recv::Msg(_) => {}
                Recv::Closed => break,
                Recv::Timeout => anyhow::bail!("the corrupted piece was not detected"),
            }
        }
        let corrupted = corrupted.context("nothing was requested")?;
        assert!(
            !have(&handle, corrupted),
            "the corrupted piece was accepted"
        );

        let seed_dir = TempDir::with_prefix("rqbit_v2_corrupt_seed")?;
        files.write_to(seed_dir.path());
        let seeder = new_session(
            seed_dir.path(),
            SessionSetup {
                listen: true,
                ..Default::default()
            },
        )
        .await?;
        let seeder_handle = add(&seeder, torrent, seed_dir.path(), vec![]).await?;
        completed(&seeder_handle, Duration::from_secs(20)).await?;
        handle
            .live()
            .context("client not live")?
            .add_peer_if_not_seen(seeder.listen_addr().context("listen addr")?)?;
        completed(&handle, Duration::from_secs(60)).await?;
        files.assert_equal_to(client_dir.path());
        client.stop().await;
        seeder.stop().await;
        Ok(())
    })
    .await
}
