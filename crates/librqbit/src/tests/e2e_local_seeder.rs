use std::{net::Ipv4Addr, sync::Arc, time::Duration};

use anyhow::Context;
use bytes::Bytes;
use tempfile::TempDir;
use tokio::{io::AsyncReadExt, time::timeout};

use crate::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, Session, SessionOptions, create_torrent,
    listen::ListenerOptions,
    spawn_utils::BlockingSpawner,
    tests::test_util::{
        TestPeerMetadata, create_default_random_dir_with_torrents, setup_test_logging, wait_until,
    },
};

struct Seeder {
    files: TempDir,
    torrent: Bytes,
    session: Arc<Session>,
}

async fn start_seeder(port: u16, num_files: usize) -> anyhow::Result<Seeder> {
    let files = create_default_random_dir_with_torrents(num_files, 8192, Some("test_seeder"));
    let torrent = create_torrent(
        files.path(),
        CreateTorrentOptions {
            piece_length: Some(1024),
            ..Default::default()
        },
        &BlockingSpawner::new(1),
    )
    .await?
    .as_bytes()?;

    let session = Session::new_with_opts(
        files.path().into(),
        SessionOptions {
            dht: None,
            persistence: None,
            peer_id: Some(TestPeerMetadata::good().as_peer_id()),
            listen: Some(ListenerOptions {
                listen_addr: (Ipv4Addr::LOCALHOST, port).into(),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await?;

    session
        .add_torrent(
            AddTorrent::from_bytes(torrent.clone()),
            Some(AddTorrentOptions {
                output_folder: Some(files.path().to_str().unwrap().to_owned()),
                overwrite: true,
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .unwrap()
        .wait_until_completed()
        .await?;

    Ok(Seeder {
        files,
        torrent,
        session,
    })
}

fn client_session_options() -> SessionOptions {
    SessionOptions {
        dht: None,
        persistence: None,
        peer_id: Some(TestPeerMetadata::good().as_peer_id()),
        ..Default::default()
    }
}

fn add_torrent_options(seeder: &Seeder, paused: bool) -> anyhow::Result<AddTorrentOptions> {
    Ok(AddTorrentOptions {
        paused,
        initial_peers: Some(vec![
            seeder
                .session
                .listen_addr()
                .context("expected listen_addr to be set")?,
        ]),
        ..Default::default()
    })
}

async fn resume_after_paused_initial_check() -> anyhow::Result<()> {
    setup_test_logging();
    let seeder = start_seeder(16002, 1).await?;
    let client_dir = TempDir::with_prefix("test_resume_after_paused_initial_check")?;
    let client = Session::new_with_opts(client_dir.path().into(), client_session_options()).await?;

    let handle = client
        .add_torrent(
            AddTorrent::from_bytes(seeder.torrent.clone()),
            Some(add_torrent_options(&seeder, true)?),
        )
        .await?
        .into_handle()
        .unwrap();
    handle.wait_until_initialized().await?;

    client.unpause(&handle).await?;
    handle.wait_until_completed().await?;

    assert_eq!(
        std::fs::read(handle.output_folder().join("0.data"))?,
        std::fs::read(seeder.files.path().join("0.data"))?
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_resume_after_paused_initial_check() -> anyhow::Result<()> {
    timeout(Duration::from_secs(10), resume_after_paused_initial_check()).await?
}

async fn move_completed_to() -> anyhow::Result<()> {
    setup_test_logging();
    let seeder = start_seeder(16003, 2).await?;
    let client_dir = TempDir::with_prefix("test_move_completed_to")?;
    let completed_dir = TempDir::with_prefix("test_move_completed_to_completed")?;
    let client = Session::new_with_opts(
        client_dir.path().into(),
        SessionOptions {
            move_completed_to: Some(completed_dir.path().into()),
            ..client_session_options()
        },
    )
    .await?;

    let handle = client
        .add_torrent(
            AddTorrent::from_bytes(seeder.torrent.clone()),
            Some(add_torrent_options(&seeder, false)?),
        )
        .await?
        .into_handle()
        .unwrap();
    let old_folder = handle.output_folder();
    let new_folder = completed_dir
        .path()
        .join(old_folder.strip_prefix(client_dir.path())?);
    handle.wait_until_completed().await?;
    wait_until(
        || {
            (handle.output_folder() == new_folder)
                .then_some(())
                .context("not moved yet")
        },
        Duration::from_secs(5),
    )
    .await?;

    assert!(!old_folder.exists());
    for name in ["0.data", "1.data"] {
        assert_eq!(
            std::fs::read(new_folder.join(name))?,
            std::fs::read(seeder.files.path().join(name))?
        );
    }

    let first_file = handle.with_metadata(|m| m.file_infos[0].relative_filename.clone())?;
    let mut streamed = Vec::new();
    handle.stream(0).await?.read_to_end(&mut streamed).await?;
    assert_eq!(
        streamed,
        std::fs::read(seeder.files.path().join(first_file))?
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_move_completed_to() -> anyhow::Result<()> {
    timeout(Duration::from_secs(10), move_completed_to()).await?
}

async fn no_pause_or_delete_while_moving() -> anyhow::Result<()> {
    setup_test_logging();
    let seeder = start_seeder(16004, 1).await?;
    let client_dir = TempDir::with_prefix("test_no_pause_or_delete_while_moving")?;
    let client = Session::new_with_opts(client_dir.path().into(), client_session_options()).await?;

    let handle = client
        .add_torrent(
            AddTorrent::from_bytes(seeder.torrent.clone()),
            Some(add_torrent_options(&seeder, false)?),
        )
        .await?
        .into_handle()
        .unwrap();
    handle.wait_until_completed().await?;

    handle.locked.write().moving = true;
    assert!(client.pause(&handle).await.is_err());
    assert!(client.delete(handle.id().into(), false).await.is_err());

    handle.locked.write().moving = false;
    client.pause(&handle).await?;
    client.delete(handle.id().into(), false).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_no_pause_or_delete_while_moving() -> anyhow::Result<()> {
    timeout(Duration::from_secs(10), no_pause_or_delete_while_moving()).await?
}
