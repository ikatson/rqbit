use std::{io::Write, time::Duration};

use crate::{
    AddTorrent, AddTorrentOptions, Session, SessionOptions, SessionPersistenceConfig,
    api::TorrentIdOrHash,
    create_torrent,
    spawn_utils::BlockingSpawner,
    tests::test_util::{create_default_random_dir_with_torrents, setup_test_logging, wait_until},
    torrent_state::{ManagedTorrentHandle, stats::TorrentStatsState},
};

async fn add_completed_torrent(
    prefix: &str,
) -> anyhow::Result<(
    tempfile::TempDir,
    tempfile::TempDir,
    std::sync::Arc<Session>,
    ManagedTorrentHandle,
)> {
    setup_test_logging();
    let files = create_default_random_dir_with_torrents(3, 65536, Some(prefix));
    let torrent = create_torrent(files.path(), Default::default(), &BlockingSpawner::new(1))
        .await?
        .as_bytes()?;
    // Use fastresume, so that the saved progress would be trusted if it wasn't for the recheck.
    let persistence = tempfile::TempDir::with_prefix(prefix)?;
    let session = Session::new_with_opts(
        files.path().into(),
        SessionOptions {
            dht: None,
            persistence: Some(SessionPersistenceConfig::Json {
                folder: Some(persistence.path().into()),
            }),
            fastresume: true,
            listen: None,
            ..Default::default()
        },
    )
    .await?;
    let handle = session
        .add_torrent(
            AddTorrent::from_bytes(torrent),
            Some(AddTorrentOptions {
                output_folder: Some(files.path().to_str().unwrap().to_owned()),
                overwrite: true,
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .unwrap();
    handle.wait_until_completed().await?;
    Ok((files, persistence, session, handle))
}

async fn wait_until_checked(handle: &ManagedTorrentHandle) -> anyhow::Result<()> {
    wait_until(
        || match handle.stats().state {
            TorrentStatsState::Initializing { .. } => anyhow::bail!("still checking"),
            _ => Ok(()),
        },
        Duration::from_secs(10),
    )
    .await
}

#[tokio::test]
async fn test_recheck_finds_corrupted_data() -> anyhow::Result<()> {
    let (files, _persistence, session, handle) =
        add_completed_torrent("test_recheck_corrupted").await?;
    let total = handle.stats().total_bytes;
    session.pause(&handle).await?;

    // Corrupt one byte. The torrent doesn't notice by itself.
    let name = handle.with_metadata(|m| m.file_infos[0].relative_filename.clone())?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(files.path().join(name))?;
    f.write_all(b"\0\0\0\0")?;
    drop(f);
    assert_eq!(handle.stats().progress_bytes, total);

    session.recheck(&handle).await?;
    wait_until_checked(&handle).await?;

    let stats = handle.stats();
    assert!(matches!(stats.state, TorrentStatsState::Paused));
    assert!(!stats.finished);
    assert!(
        stats.progress_bytes < total,
        "{} < {}",
        stats.progress_bytes,
        total
    );

    // Rechecking while a check is running is refused.
    session.recheck(&handle).await?;
    assert!(session.recheck(&handle).await.is_err());

    session
        .delete(TorrentIdOrHash::Id(handle.id()), false)
        .await?;
    Ok(())
}

#[tokio::test]
async fn test_recheck_live_torrent_stays_live() -> anyhow::Result<()> {
    let (_files, _persistence, session, handle) =
        add_completed_torrent("test_recheck_live").await?;
    let total = handle.stats().total_bytes;

    session.recheck(&handle).await?;
    wait_until_checked(&handle).await?;

    let stats = handle.stats();
    assert!(matches!(stats.state, TorrentStatsState::Live));
    assert!(stats.finished);
    assert_eq!(stats.progress_bytes, total);
    Ok(())
}
