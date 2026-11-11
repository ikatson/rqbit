//! BitTorrent v2 (BEP 52) magnet links: `btmh` alone (v2-only, or a hybrid torrent known by
//! its v2 hash only) and `btih` with `btmh` (hybrid). The info dictionary is read from peers
//! and checked against every hash of the link, the piece layers are fetched afterwards, and
//! what is stored lets a restart do without both.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::Path,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, ensure};
use bytes::Bytes;
use librqbit_core::torrent_metainfo::torrent_from_bytes;
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tracing::debug;

use crate::{
    AddTorrent, AddTorrentOptions, Session,
    api::TorrentIdOrHash,
    tests::{
        e2e_v2::{PIECE_LEN, SessionSetup, completed, new_session, sample_files},
        e2e_v2_hashes::{have_all_piece_layers, start_seeder, wait_for_stored_layers},
        fake_peer::{FakePeer, peer_id, run},
    },
    torrent_state::ManagedTorrentHandle,
};

/// The id rqbit gives `ut_metadata` in its extended handshake: the one a peer answers with.
const OUR_UT_METADATA: u8 = 3;
const METADATA_PIECE: usize = 16 * 1024;
/// Extension protocol (BEP 10) and BitTorrent v2 (BEP 52) bits.
const V2_RESERVED: [u8; 8] = [0, 0, 0, 0, 0, 0x10, 0, 0x10];

pub(super) async fn add_magnet(
    session: &Arc<Session>,
    magnet: &str,
    output_folder: &Path,
    peers: Vec<SocketAddr>,
) -> anyhow::Result<ManagedTorrentHandle> {
    session
        .add_torrent(
            AddTorrent::from_url(magnet.to_owned()),
            Some(AddTorrentOptions {
                output_folder: Some(output_folder.to_str().context("utf-8 path")?.to_owned()),
                overwrite: true,
                initial_peers: Some(peers),
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .context("into_handle")
}

/// A torrent added from a `btmh` magnet link, alone or with `btih`, downloads from a seeder
/// session: metadata, then piece layers, then data. A restart with the seeder gone finds
/// the torrent complete with every piece layer: neither is fetched again.
#[tokio::test(flavor = "multi_thread")]
async fn test_v2_magnets_download_and_survive_a_restart() -> anyhow::Result<()> {
    run(async {
        // (hybrid torrent, `btih` in the link)
        for (seed, (hybrid, with_btih)) in [(false, false), (true, true), (true, false)]
            .into_iter()
            .enumerate()
        {
            let case = format!("hybrid={hybrid} btih={with_btih}");
            let files = sample_files(40 + seed as u64);
            let full = Bytes::from(files.torrent("v2", PIECE_LEN, hybrid).to_bytes());
            let parsed = torrent_from_bytes(&full)?;
            let info_hash = parsed.info_hash;
            let v2 = parsed.info_hash_v2.context("no v2 hash")?;
            let mut magnet = format!("magnet:?xt=urn:btmh:1220{}", v2.as_string());
            if with_btih {
                magnet.push_str(&format!("&xt=urn:btih:{}", info_hash.as_string()));
            }
            magnet.push_str("&dn=v2");

            let (_seed_dir, seeder, _seeder_handle) = start_seeder(&files, full.clone()).await?;
            let seeder_addr = seeder.listen_addr().context("addr")?;

            let client_dir = TempDir::with_prefix("rqbit_v2_magnet")?;
            let out = client_dir.path().join("out");
            let persistence = client_dir.path().join("session");
            let setup = || SessionSetup {
                persistence: Some(persistence.clone()),
                ..Default::default()
            };
            let client = new_session(&out, setup()).await?;
            let handle = add_magnet(&client, &magnet, &out, vec![seeder_addr]).await?;
            // A hybrid torrent is known by its v1 hash, even from a `btmh`-only link.
            assert_eq!(handle.info_hash(), info_hash, "{case}");
            assert_eq!(handle.info_hash_v2(), Some(v2), "{case}");
            completed(&handle, Duration::from_secs(60)).await?;
            files.assert_equal_to(&out);
            assert!(have_all_piece_layers(&handle), "{case}");
            wait_for_stored_layers(&persistence, Duration::from_secs(10)).await?;
            client.stop().await;
            drop(handle);
            drop(client);
            seeder.stop().await;

            let client = new_session(&out, setup()).await?;
            let handle = client
                .get(TorrentIdOrHash::Hash(info_hash))
                .with_context(|| format!("{case}: torrent not restored"))?;
            completed(&handle, Duration::from_secs(20)).await?;
            assert!(have_all_piece_layers(&handle), "{case}");
            assert_eq!(handle.info_hash_v2(), Some(v2), "{case}");
            files.assert_equal_to(&out);
            client.stop().await;
        }
        Ok(())
    })
    .await
}

/// Forwards every connection to `listener` to `target` once `open` turns true: a peer
/// that is only reachable after the test says so.
fn gated_relay(
    listener: TcpListener,
    target: SocketAddr,
    open: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let mut inbound = match listener.accept().await {
                Ok((s, _)) => s,
                Err(e) => {
                    debug!("relay accept: {e:#}");
                    return;
                }
            };
            let mut open = open.clone();
            tokio::spawn(async move {
                if open.wait_for(|o| *o).await.is_err() {
                    return;
                }
                let relayed = async {
                    let mut out = TcpStream::connect(target).await?;
                    tokio::io::copy_bidirectional(&mut inbound, &mut out).await
                };
                if let Err(e) = relayed.await {
                    debug!("relay: {e:#}");
                }
            });
        }
    })
}

/// A peer that serves an info dictionary that does not hash to the `btmh` value of the
/// link is dropped, and the metadata comes from the honest seeder.
#[tokio::test(flavor = "multi_thread")]
async fn test_v2_magnet_forged_info_dict_is_rejected() -> anyhow::Result<()> {
    run(async {
        let files = sample_files(50);
        let full = Bytes::from(files.torrent("v2", PIECE_LEN, false).to_bytes());
        let parsed = torrent_from_bytes(&full)?;
        let info_hash = parsed.info_hash;
        let v2 = parsed.info_hash_v2.context("no v2 hash")?;
        let magnet = format!("magnet:?xt=urn:btmh:1220{}", v2.as_string());
        // A well-formed v2 info dictionary, of another torrent.
        let other = Bytes::from(sample_files(51).torrent("v2", PIECE_LEN, false).to_bytes());
        let forged = torrent_from_bytes(&other)?.info.raw_bytes.as_ref().to_vec();
        ensure!(
            forged.len() <= METADATA_PIECE,
            "the forged info is one piece"
        );

        let (_seed_dir, seeder, _seeder_handle) = start_seeder(&files, full).await?;
        let forger_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let forger_addr = forger_listener.local_addr()?;
        let relay = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let relay_addr = relay.local_addr()?;
        let (open_tx, open_rx) = tokio::sync::watch::channel(false);
        let relay_task = gated_relay(relay, seeder.listen_addr().context("addr")?, open_rx);

        let client_dir = TempDir::with_prefix("rqbit_v2_magnet_forged")?;
        let client = new_session(client_dir.path(), SessionSetup::default()).await?;
        let adding = tokio::spawn({
            let client = client.clone();
            let out = client_dir.path().to_owned();
            async move { add_magnet(&client, &magnet, &out, vec![forger_addr, relay_addr]).await }
        });

        let (mut forger, theirs) =
            FakePeer::accept(&forger_listener, &peer_id(b"-FG0001-"), V2_RESERVED).await?;
        // A `btmh`-only link joins the swarm of the truncated v2 hash, as a v2 peer.
        assert_eq!(&theirs[28..48], info_hash.0.as_slice());
        assert_eq!(info_hash.0, v2.0[..20]);
        assert_ne!(theirs[27] & 0x10, 0, "no v2 bit in the handshake");
        forger
            .extended_handshake(
                format!(
                    "d1:md11:ut_metadatai{OUR_UT_METADATA}ee13:metadata_sizei{}ee",
                    forged.len()
                )
                .as_bytes(),
            )
            .await?;
        let mut data = vec![OUR_UT_METADATA];
        data.extend_from_slice(
            format!("d8:msg_typei1e5:piecei0e10:total_sizei{}ee", forged.len()).as_bytes(),
        );
        data.extend_from_slice(&forged);
        forger.send(20, &data).await?;
        forger
            .expect_closed(Duration::from_secs(10))
            .await
            .context("the peer with the forged info dictionary was not dropped")?;

        open_tx.send(true)?;
        let handle = adding.await??;
        assert_eq!(handle.info_hash_v2(), Some(v2));
        completed(&handle, Duration::from_secs(60)).await?;
        files.assert_equal_to(client_dir.path());
        assert!(have_all_piece_layers(&handle));
        client.stop().await;
        seeder.stop().await;
        relay_task.abort();
        Ok(())
    })
    .await
}
