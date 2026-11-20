use crate::{
    AddTorrent, AddTorrentOptions, Session, SessionOptions, create_torrent,
    spawn_utils::BlockingSpawner,
    tests::test_util::{create_default_random_dir_with_torrents, setup_test_logging},
};

#[tokio::test]
async fn test_reannounce_needs_live_torrent() -> anyhow::Result<()> {
    setup_test_logging();
    let files = create_default_random_dir_with_torrents(1, 16384, Some("test_reannounce"));
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
    let handle = session
        .add_torrent(
            AddTorrent::from_bytes(torrent),
            Some(AddTorrentOptions {
                output_folder: Some(files.path().to_str().unwrap().to_owned()),
                overwrite: true,
                paused: true,
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .unwrap();
    handle.wait_until_initialized().await?;
    assert!(handle.reannounce().is_err());

    session.unpause(&handle).await?;
    handle.reannounce()?;

    session.pause(&handle).await?;
    assert!(handle.reannounce().is_err());
    Ok(())
}
