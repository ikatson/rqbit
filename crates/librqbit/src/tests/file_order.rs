use crate::{
    AddTorrent, AddTorrentOptions, Session, SessionOptions, create_torrent,
    spawn_utils::BlockingSpawner,
    tests::test_util::{create_default_random_dir_with_torrents, setup_test_logging},
};

async fn make_session_and_torrent(
    prefix: &str,
) -> anyhow::Result<(tempfile::TempDir, std::sync::Arc<Session>, bytes::Bytes)> {
    setup_test_logging();
    let files = create_default_random_dir_with_torrents(3, 16384, Some(prefix));
    let torrent = create_torrent(files.path(), Default::default(), &BlockingSpawner::new(1))
        .await?
        .as_bytes()?;
    let session = Session::new_with_opts(
        files.path().into(),
        SessionOptions {
            dht: None,
            persistence: None,
            listen: None,
            ..Default::default()
        },
    )
    .await?;
    Ok((files, session, torrent))
}

fn opts(files: &tempfile::TempDir, file_order: Option<Vec<usize>>) -> Option<AddTorrentOptions> {
    Some(AddTorrentOptions {
        output_folder: Some(files.path().to_str().unwrap().to_owned()),
        overwrite: true,
        file_order,
        ..Default::default()
    })
}

#[tokio::test]
async fn test_add_torrent_rejects_invalid_file_order() -> anyhow::Result<()> {
    let (files, session, torrent) = make_session_and_torrent("test_file_order_invalid").await?;
    let res = session
        .add_torrent(
            AddTorrent::from_bytes(torrent),
            opts(&files, Some(vec![0, 3])),
        )
        .await;
    assert!(res.is_err());
    Ok(())
}

#[tokio::test]
async fn test_update_file_order() -> anyhow::Result<()> {
    let (files, session, torrent) = make_session_and_torrent("test_file_order_update").await?;
    let handle = session
        .add_torrent(AddTorrent::from_bytes(torrent), opts(&files, Some(vec![2])))
        .await?
        .into_handle()
        .unwrap();
    assert_eq!(handle.file_order(), Some(vec![2]));
    handle.wait_until_completed().await?;

    let live_priorities = |handle: &crate::torrent_state::ManagedTorrentHandle| {
        handle
            .live()
            .unwrap()
            .lock_read("test")
            .file_priorities
            .clone()
    };
    assert_eq!(live_priorities(&handle), vec![2, 0, 1]);

    // Live torrent: the picker's order changes right away.
    handle.update_file_order(Some(vec![1, 2]))?;
    assert_eq!(handle.file_order(), Some(vec![1, 2]));
    assert_eq!(live_priorities(&handle), vec![1, 2, 0]);

    // Invalid order is rejected and leaves the old one in place.
    assert!(handle.update_file_order(Some(vec![5])).is_err());
    assert_eq!(handle.file_order(), Some(vec![1, 2]));

    // Paused torrent: the order is kept and used when it goes live again.
    session.pause(&handle).await?;
    handle.update_file_order(Some(vec![0, 2]))?;
    session.unpause(&handle).await?;
    assert_eq!(live_priorities(&handle), vec![0, 2, 1]);

    handle.update_file_order(None)?;
    assert_eq!(handle.file_order(), None);
    assert_eq!(live_priorities(&handle), vec![0, 1, 2]);
    Ok(())
}
