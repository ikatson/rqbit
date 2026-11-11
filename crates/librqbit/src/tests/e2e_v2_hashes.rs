//! BitTorrent v2 (BEP 52) hash requests between a session and wire-level peers or other
//! sessions: the answers a seeder gives, the bounds on what a peer may ask, and a torrent
//! added without `piece layers` fetching them from its peers.

use std::{
    collections::HashSet,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, bail, ensure};
use bytes::Bytes;
use librqbit_core::{
    Id20, Id32,
    merkle::{self, HashRequestShape},
    torrent_metainfo::torrent_from_bytes,
};
use tempfile::TempDir;

use crate::{
    Session,
    api::TorrentIdOrHash,
    tests::{
        e2e_v2::{
            PIECE_LEN, SessionSetup, add, completed, connect_with_hash, new_session, sample_files,
        },
        fake_peer::{FakePeer, Recv, Wire, run},
        v2_torrent::V2Files,
    },
    torrent_state::{
        ManagedTorrentHandle,
        live::hashes::{ANSWER_BURST, ANSWERS_PER_SECOND, MAX_REFUSED_REQUESTS},
    },
};

const MSGID_HASH_REQUEST: u8 = 21;
const MSGID_HASHES: u8 = 22;

fn hash_request(root: &Id32, base: u32, index: u32, length: u32, proof_layers: u32) -> Vec<u8> {
    let mut p = root.0.to_vec();
    for v in [base, index, length, proof_layers] {
        p.extend_from_slice(&v.to_be_bytes());
    }
    p
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// The pieces root and geometry of the 48 request bytes that start a hash message.
fn parse_request(p: &[u8]) -> (Id32, HashRequestShape) {
    let mut root = Id32::default();
    root.0.copy_from_slice(&p[..32]);
    let shape = HashRequestShape {
        base: be32(&p[32..]),
        index: be32(&p[36..]),
        count: be32(&p[40..]),
        proof_layers: be32(&p[44..]),
    };
    (root, shape)
}

/// The request a `hashes` payload answers and its hashes.
fn parse_hashes(p: &[u8]) -> (Id32, HashRequestShape, Vec<Id32>) {
    let (root, shape) = parse_request(p);
    let hashes = p[48..]
        .as_chunks::<32>()
        .0
        .iter()
        .map(|c| Id32::new(*c))
        .collect();
    (root, shape, hashes)
}

/// The next hash message of `peer`, skipping everything else.
async fn next_hash_message(peer: &mut FakePeer, within: Duration) -> anyhow::Result<Wire> {
    let deadline = Instant::now() + within;
    loop {
        match peer
            .recv(deadline.saturating_duration_since(Instant::now()))
            .await
        {
            Recv::Msg(m @ (Wire::Hashes(_) | Wire::HashReject(_) | Wire::HashRequest(_))) => {
                return Ok(m);
            }
            Recv::Msg(_) => {}
            Recv::Timeout => bail!("no hash message in {within:?}"),
            Recv::Closed => bail!("closed before a hash message"),
        }
    }
}

/// A session that seeds `files` with `torrent` (which has every piece layer).
pub(super) async fn start_seeder(
    files: &V2Files,
    torrent: Bytes,
) -> anyhow::Result<(TempDir, Arc<Session>, ManagedTorrentHandle)> {
    let dir = TempDir::with_prefix("rqbit_v2_hashes_seed")?;
    files.write_to(dir.path());
    let session = new_session(
        dir.path(),
        SessionSetup {
            listen: true,
            ..Default::default()
        },
    )
    .await?;
    let handle = add(&session, torrent, dir.path(), vec![]).await?;
    completed(&handle, Duration::from_secs(60))
        .await
        .context("the seeder's own files did not verify")?;
    Ok((dir, session, handle))
}

fn hash_requests_received(handle: &ManagedTorrentHandle) -> u64 {
    handle.live().map_or(0, |l| l.hash_requests_received())
}

/// `torrent` without its `piece layers`, the way it is known after resolving a magnet.
fn without_piece_layers(files: &V2Files, name: &str, hybrid: bool) -> Bytes {
    let mut t = files.torrent(name, PIECE_LEN, hybrid);
    t.remove("piece layers")
        .expect("the torrent had piece layers");
    Bytes::from(t.to_bytes())
}

/// Pieces whose file is at most one piece long: verified against the file's pieces root,
/// so they can be downloaded before any piece layer is known.
fn pieces_of_small_files(files: &V2Files) -> HashSet<u32> {
    let mut out = HashSet::new();
    let mut first = 0u32;
    for (_, content) in &files.files {
        let n = u32::try_from(content.len().div_ceil(PIECE_LEN as usize)).unwrap();
        if content.len() <= PIECE_LEN as usize {
            out.extend(first..first + n);
        }
        first += n;
    }
    out
}

pub(super) fn have_all_piece_layers(handle: &ManagedTorrentHandle) -> bool {
    handle
        .metadata
        .load()
        .as_ref()
        .is_some_and(|m| m.info.has_all_piece_layers())
}

/// A seeder answers valid hash requests of a v2 peer with hashes whose proofs check against
/// the pieces root, and rejects requests it cannot answer: unknown root, the block layer,
/// a count that is not a power of two, a range past the layer, a file without a piece layer.
#[tokio::test(flavor = "multi_thread")]
async fn test_v2_seeder_answers_hash_requests() -> anyhow::Result<()> {
    run(async {
        let pl = PIECE_LEN as usize;
        let files = V2Files::random(
            &[
                ("big.bin", 3 * pl + 5000),
                ("huge.bin", 20 * pl + 3),
                ("small.bin", 1000),
            ],
            31,
        );
        let torrent = Bytes::from(files.torrent("v2", PIECE_LEN, false).to_bytes());
        let info_hash: Id20 = torrent_from_bytes(&torrent)?.info_hash;
        let (_dir, seeder, handle) = start_seeder(&files, torrent).await?;
        let addr = seeder.listen_addr().context("listen addr")?;
        let layers = files.piece_layers(PIECE_LEN);
        let len_of = |name: &str| -> u64 {
            files.files.iter().find(|(p, _)| p == name).unwrap().1.len() as u64
        };
        let root_of = |name: &str| {
            merkle::pieces_root_from_data(&files.files.iter().find(|(p, _)| p == name).unwrap().1)
                .unwrap()
        };
        let layer_of = |root: &Id32| layers.iter().find(|(r, _)| r == root).unwrap().1.clone();

        let (mut peer, reply) = connect_with_hash(addr, info_hash, true).await?;
        assert!(reply.supports_v2(), "the seeder did not set the v2 bit");

        // big.bin: 7 blocks, a tree of height 3, a piece layer (height 1) of 4 hashes.
        // huge.bin: 41 blocks, a tree of height 6, a piece layer of 32 hashes (21 real).
        let valid = [
            ("big.bin", 1, 0, 4, 1),
            ("big.bin", 1, 2, 2, 1),
            ("big.bin", 1, 0, 2, 0),
            ("huge.bin", 1, 0, 32, 4),
            ("huge.bin", 1, 8, 8, 4),
            ("huge.bin", 1, 16, 16, 2),
            ("huge.bin", 2, 4, 4, 3),
        ];
        for (name, base, index, length, proof_layers) in valid {
            let root = root_of(name);
            peer.send(
                MSGID_HASH_REQUEST,
                &hash_request(&root, base, index, length, proof_layers),
            )
            .await?;
            let Wire::Hashes(p) = next_hash_message(&mut peer, Duration::from_secs(10)).await?
            else {
                bail!("{name} {base}/{index}/{length}/{proof_layers} was not answered");
            };
            let (got_root, shape, hashes) = parse_hashes(&p);
            assert_eq!(got_root, root);
            assert_eq!(
                (shape.base, shape.index, shape.count, shape.proof_layers),
                (base, index, length, proof_layers)
            );
            assert_eq!(hashes.len(), shape.response_len() as usize);
            // With the proof up to the root (one layer below it), the hashes check against
            // the pieces root; a shorter proof is answered as asked.
            let tree_height = merkle::num_leafs(len_of(name)).unwrap().trailing_zeros();
            if proof_layers + 1 == tree_height - base {
                assert!(
                    merkle::verify_hashes(&root, len_of(name), &shape, &hashes),
                    "{name} {base}/{index}/{length}/{proof_layers}: the proof does not check"
                );
            }
            if base == 1 {
                // The base hashes are the piece layer, padded with the hashes of empty pieces.
                let layer = layer_of(&root);
                for (i, h) in hashes[..length as usize].iter().enumerate() {
                    if let Some(want) = layer.get(index as usize + i) {
                        assert_eq!(h, want, "{name}: piece {} hash", index as usize + i);
                    }
                }
            }
        }

        let invalid = [
            (Id32::new([7; 32]), 1, 0, 4, 1),
            // The block layer: we do not keep it.
            (root_of("big.bin"), 0, 0, 8, 2),
            (root_of("big.bin"), 1, 0, 3, 1),
            (root_of("big.bin"), 1, 4, 2, 1),
            (root_of("big.bin"), 1, 1, 2, 1),
            (root_of("big.bin"), 1, 0, 1, 1),
            (root_of("huge.bin"), 1, 0, 64, 4),
            (root_of("small.bin"), 1, 0, 2, 0),
        ];
        for (root, base, index, length, proof_layers) in invalid {
            let request = hash_request(&root, base, index, length, proof_layers);
            peer.send(MSGID_HASH_REQUEST, &request).await?;
            match next_hash_message(&mut peer, Duration::from_secs(10)).await? {
                Wire::HashReject(p) => assert_eq!(p, request),
                other => bail!("{base}/{index}/{length}/{proof_layers}: got {other:?}"),
            }
        }
        assert_eq!(
            hash_requests_received(&handle),
            (valid.len() + invalid.len()) as u64
        );

        seeder.stop().await;

        // Every connection of a v2-only torrent is a v2 one; a v1 connection to a hybrid
        // torrent (the v1 hash, without the "supports v2" bit) may not carry hash messages.
        let hybrid = Bytes::from(files.torrent("hybrid", PIECE_LEN, true).to_bytes());
        let v1_hash = torrent_from_bytes(&hybrid)?.info_hash;
        let (_hybrid_dir, seeder, _) = start_seeder(&files, hybrid).await?;
        let addr = seeder.listen_addr().context("listen addr")?;
        let (mut v1_peer, reply) = connect_with_hash(addr, v1_hash, false).await?;
        assert_eq!(reply.info_hash, v1_hash, "a v1 connection was upgraded");
        v1_peer
            .send(
                MSGID_HASH_REQUEST,
                &hash_request(&root_of("big.bin"), 1, 0, 4, 1),
            )
            .await?;
        v1_peer.expect_closed(Duration::from_secs(10)).await?;
        seeder.stop().await;
        Ok(())
    })
    .await
}

/// A peer flooding hash requests gets at most its budget answered, rejects for the rest,
/// and is disconnected once it keeps asking far past its budget.
#[tokio::test(flavor = "multi_thread")]
async fn test_v2_hash_request_flood_is_bounded() -> anyhow::Result<()> {
    run(async {
        let files = sample_files(32);
        let torrent = Bytes::from(files.torrent("v2", PIECE_LEN, false).to_bytes());
        let info_hash: Id20 = torrent_from_bytes(&torrent)?.info_hash;
        let (_dir, seeder, _handle) = start_seeder(&files, torrent).await?;
        let (root, _) = files.piece_layers(PIECE_LEN)[0].clone();
        let (mut peer, _) =
            connect_with_hash(seeder.listen_addr().context("addr")?, info_hash, true).await?;

        let request = FakePeer::frame(MSGID_HASH_REQUEST, &hash_request(&root, 1, 0, 2, 0));
        let flood = 3 * (ANSWER_BURST + MAX_REFUSED_REQUESTS) as usize;
        let start = Instant::now();
        peer.send_raw(&request.repeat(flood)).await?;
        let (msgs, closed) = peer.collect(Duration::from_secs(10)).await;
        let elapsed = start.elapsed();
        let answered = msgs.iter().filter(|m| matches!(m, Wire::Hashes(_))).count();
        let rejected = msgs
            .iter()
            .filter(|m| matches!(m, Wire::HashReject(_)))
            .count();
        assert!(closed, "the flooding peer was not disconnected");
        // The budget refills while the flood is read.
        let refill = u128::from(ANSWERS_PER_SECOND) * elapsed.as_millis() / 1000 + 1;
        assert!(
            answered as u128 <= u128::from(ANSWER_BURST) + refill,
            "{answered} answers in {elapsed:?}"
        );
        assert!(answered >= 1, "nothing was answered");
        assert!(
            rejected <= MAX_REFUSED_REQUESTS as usize,
            "{rejected} rejects before the disconnect"
        );
        assert!(answered + rejected < flood, "the whole flood was handled");
        seeder.stop().await;
        Ok(())
    })
    .await
}

/// A torrent added without `piece layers` downloads only the pieces it can check (those of
/// files of at most one piece) until it knows the layers. It asks its v2 peers for them in
/// chunks of 512 hashes: a peer answering with a forged proof is dropped and the layers come
/// from an honest seeder, after which everything downloads.
#[tokio::test(flavor = "multi_thread")]
async fn test_v2_piece_layers_fetched_from_peers_forged_proof_rejected() -> anyhow::Result<()> {
    run(async {
        let pl = PIECE_LEN as usize;
        // 601 pieces: a layer of 1024 hashes, asked in two chunks of 512.
        let files = V2Files::random(
            &[
                ("a.bin", 600 * pl + 11),
                ("b/c.bin", 3 * pl + 5),
                ("d.bin", 1000),
            ],
            33,
        );
        let full = Bytes::from(files.torrent("v2", PIECE_LEN, false).to_bytes());
        let stripped = without_piece_layers(&files, "v2", false);
        let parsed = torrent_from_bytes(&stripped)?;
        assert!(parsed.validate_with_piece_layers().is_err());
        let info_hash = parsed.info_hash;
        let num_pieces = u32::try_from(files.v2_pieces(PIECE_LEN).len())?;
        let small = pieces_of_small_files(&files);
        let a_root = merkle::pieces_root_from_data(&files.files[0].1).context("a.bin root")?;
        assert_eq!(files.files[0].0, "a.bin");

        let client_dir = TempDir::with_prefix("rqbit_v2_hashes_client")?;
        let client = new_session(
            client_dir.path(),
            SessionSetup {
                listen: true,
                ..Default::default()
            },
        )
        .await?;
        let handle = add(&client, stripped, client_dir.path(), vec![]).await?;
        handle.wait_until_initialized().await?;
        assert!(!have_all_piece_layers(&handle));

        // A peer with every piece that forges the proofs of the hashes it is asked for.
        let (mut forger, reply) =
            connect_with_hash(client.listen_addr().context("addr")?, info_hash, true).await?;
        assert!(reply.supports_v2());
        let mut bits = vec![0u8; num_pieces.div_ceil(8) as usize];
        for i in 0..num_pieces {
            bits[(i / 8) as usize] |= 0x80 >> (i % 8);
        }
        forger.bitfield(&bits).await?;
        forger.unchoke().await?;
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut forged = 0;
        loop {
            match forger
                .recv(deadline.saturating_duration_since(Instant::now()))
                .await
            {
                Recv::Msg(Wire::HashRequest(p)) => {
                    let (root, shape) = parse_request(&p);
                    if root == a_root {
                        assert_eq!(shape.count, 512, "a.bin's layer asked in chunks of 512");
                    }
                    let mut answer = p.clone();
                    answer.resize(48 + 32 * shape.response_len() as usize, 0xab);
                    if forger.send(MSGID_HASHES, &answer).await.is_err() {
                        break;
                    }
                    forged += 1;
                }
                Recv::Msg(Wire::Request(index, ..)) => ensure!(
                    small.contains(&index),
                    "piece {index} was requested before its piece layer was known"
                ),
                Recv::Msg(_) => {}
                Recv::Closed => break,
                Recv::Timeout => bail!("the peer forging hashes was not dropped"),
            }
        }
        ensure!(forged >= 1, "no hash request reached the forging peer");
        assert!(!have_all_piece_layers(&handle));

        let (_seed_dir, seeder, seeder_handle) = start_seeder(&files, full).await?;
        handle
            .live()
            .context("client not live")?
            .add_peer_if_not_seen(seeder.listen_addr().context("addr")?)?;
        completed(&handle, Duration::from_secs(120)).await?;
        files.assert_equal_to(client_dir.path());
        assert!(have_all_piece_layers(&handle));
        // a.bin in two chunks, b/c.bin in one.
        assert!(hash_requests_received(&seeder_handle) >= 3);

        // The stored torrent bytes now carry the layers, the info hash unchanged.
        let bytes = handle
            .metadata
            .load()
            .as_ref()
            .context("no metadata")?
            .torrent_bytes
            .clone();
        let reparsed = torrent_from_bytes(&bytes)?;
        assert_eq!(reparsed.info_hash, info_hash);
        reparsed.validate_with_piece_layers()?;
        client.stop().await;
        seeder.stop().await;
        Ok(())
    })
    .await
}

/// The `.torrent` file in `persistence` once it holds every piece layer.
pub(super) async fn wait_for_stored_layers(
    persistence: &Path,
    within: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + within;
    loop {
        for e in std::fs::read_dir(persistence)?.filter_map(|e| e.ok()) {
            if !e.file_name().to_string_lossy().ends_with(".torrent") {
                continue;
            }
            let bytes = std::fs::read(e.path())?;
            if torrent_from_bytes(&bytes).is_ok_and(|t| t.validate_with_piece_layers().is_ok()) {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            bail!("the learned piece layers were not stored in {within:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Piece layers learned from peers are stored with the torrent: after a restart the torrent
/// has them and asks no peer for them again.
#[tokio::test(flavor = "multi_thread")]
async fn test_v2_fetched_piece_layers_survive_a_restart() -> anyhow::Result<()> {
    run(async {
        for hybrid in [false, true] {
            let files = sample_files(34);
            let full = Bytes::from(files.torrent("v2", PIECE_LEN, hybrid).to_bytes());
            let stripped = without_piece_layers(&files, "v2", hybrid);
            let info_hash = torrent_from_bytes(&stripped)?.info_hash;
            let (_seed_dir, seeder, seeder_handle) = start_seeder(&files, full).await?;
            let seeder_addr = seeder.listen_addr().context("addr")?;

            let client_dir = TempDir::with_prefix("rqbit_v2_hashes_restart")?;
            let out = client_dir.path().join("out");
            let persistence = client_dir.path().join("session");
            let setup = || SessionSetup {
                listen: true,
                persistence: Some(persistence.clone()),
                ..Default::default()
            };
            let client = new_session(&out, setup()).await?;
            let handle = add(&client, stripped, &out, vec![seeder_addr]).await?;
            completed(&handle, Duration::from_secs(60)).await?;
            files.assert_equal_to(&out);
            assert!(have_all_piece_layers(&handle));
            wait_for_stored_layers(&persistence, Duration::from_secs(10)).await?;
            let asked = hash_requests_received(&seeder_handle);
            assert!(asked >= 1, "hybrid={hybrid}: the seeder was never asked");
            client.stop().await;
            drop(handle);
            drop(client);

            let client = new_session(&out, setup()).await?;
            let handle = client
                .get(TorrentIdOrHash::Hash(info_hash))
                .context("torrent not restored")?;
            completed(&handle, Duration::from_secs(20)).await?;
            assert!(have_all_piece_layers(&handle), "hybrid={hybrid}");
            let live = handle.live().context("not live")?;
            live.add_peer_if_not_seen(seeder_addr)?;
            tokio::time::sleep(Duration::from_secs(3)).await;
            assert_eq!(live.hash_requests_sent(), 0, "hybrid={hybrid}");
            assert_eq!(
                hash_requests_received(&seeder_handle),
                asked,
                "hybrid={hybrid}: the layers were asked for again"
            );
            files.assert_equal_to(&out);
            client.stop().await;
            seeder.stop().await;
        }
        Ok(())
    })
    .await
}
