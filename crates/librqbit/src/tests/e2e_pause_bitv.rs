use std::{
    collections::HashSet,
    net::Ipv4Addr,
    time::{Duration, Instant},
};

use anyhow::Context;
use tempfile::TempDir;

use super::fake_peer::{FakePeer, Recv, SeederOpts, Wire, peer_id, run, start_seeder};
use crate::{
    AddTorrent, ConnectionOptions, ListenerOptions, Session, SessionOptions,
    SessionPersistenceConfig,
};

// Pausing saves the pieces verified since the last periodic flush of the bitfield, so a
// restart doesn't download them again.
#[tokio::test(flavor = "multi_thread")]
async fn test_pause_saves_verified_pieces_to_bitv() -> anyhow::Result<()> {
    run(async {
        let seed = start_seeder(SeederOpts::default()).await?;

        let dir = TempDir::with_prefix("rqbit_pause_bitv")?;
        let state = dir.path().join("state");
        let session = Session::new_with_opts(
            dir.path().join("download"),
            SessionOptions {
                dht: None,
                fastresume: true,
                persistence: Some(SessionPersistenceConfig::Json {
                    folder: Some(state.clone()),
                }),
                listen: Some(ListenerOptions {
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
        .await?;
        let handle = session
            .add_torrent(AddTorrent::from_bytes(seed.torrent_bytes.clone()), None)
            .await?
            .into_handle()
            .context("into_handle")?;
        handle.wait_until_initialized().await?;

        // The peer serves pieces 0..=2 of 4, so the torrent isn't finished, and far less than
        // the periodic flush threshold is verified.
        let addr = session.listen_addr().context("client is not listening")?;
        let mut peer = FakePeer::connect(addr, seed.info_hash, &peer_id(b"-FK0001-")).await?;
        peer.bitfield(&[0b1110_0000]).await?;
        peer.unchoke().await?;

        let mut haves = HashSet::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while haves.len() < 3 {
            let left = deadline.saturating_duration_since(Instant::now());
            match peer.recv(left).await {
                Recv::Msg(Wire::Request(piece, begin, len)) => {
                    peer.piece(piece, begin, seed.block(piece, begin, len))
                        .await?;
                }
                Recv::Msg(Wire::Have(piece)) => {
                    haves.insert(piece);
                }
                Recv::Msg(_) => {}
                Recv::Timeout | Recv::Closed => anyhow::bail!("got HAVE only for {haves:?}"),
            }
        }

        session.pause(&handle).await?;

        let bitv = std::fs::read(state.join(format!("{:?}.bitv", seed.info_hash)))?;
        assert_eq!(
            bitv.first(),
            Some(&0b1110_0000),
            "pieces saved in the .bitv"
        );
        Ok(())
    })
    .await
}
