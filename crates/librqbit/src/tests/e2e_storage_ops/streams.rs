//! Streams that read while a storage operation runs on their torrent: the read waits for the
//! operation to end, then reads from where the files are now, and never returns bytes that a
//! recheck found damaged.
//!
//! Each test opens a stream, holds the operation at a gate, and polls a read of the whole
//! file once by hand: the read must be pending then, and complete with the file's bytes once
//! the gate opens.

use std::pin::pin;

use tokio::io::AsyncReadExt;

use super::*;

// In nested directories, followed by a padding file (with `pad`).
const STREAMED: &str = "sub/deeper/b.bin";

// `content` written to `folder` and added, complete, to a new session in `dir` with storage
// made by `factory`.
async fn add_complete(
    dir: &Path,
    folder: &Path,
    content: &Content,
    factory: BoxStorageFactory,
    initial_peers: Option<Vec<SocketAddr>>,
) -> anyhow::Result<(Arc<Session>, ManagedTorrentHandle)> {
    content.write_to(folder);
    let session = new_session(dir, Default::default()).await?;
    let handle = add(
        &session,
        content,
        folder,
        AddTorrentOptions {
            storage_factory: Some(factory),
            initial_peers,
            ..Default::default()
        },
    )
    .await?;
    wait_completed(&handle).await?;
    assert_eq!(have_pieces(&handle)?, content.all_pieces());
    Ok((session, handle))
}

// A read while a move has the storage waits for the move, then reads the file at its new
// place.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_stream_read_waits_for_move() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let file = content.file(STREAMED);
        let dir = TempDir::with_prefix("rqbit_stream_move")?;
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        let ctl = Arc::new(Ctl::default());
        let (session, handle) = add_complete(
            dir.path(),
            &old,
            &content,
            InstrumentedFactory { ctl: ctl.clone() }.boxed(),
            None,
        )
        .await?;
        let mut stream = handle.clone().stream(file.index).await?;

        ctl.move_gate.arm();
        let mv = tokio::spawn({
            let session = session.clone();
            let handle = handle.clone();
            let new = new.clone();
            async move {
                session
                    .move_storage(&handle, new, MoveStorageMode::FailIfExists)
                    .await
            }
        });
        ctl.move_gate.wait_entered().await?;

        let mut buf = vec![0u8; file.data.len()];
        {
            let mut read = pin!(stream.read_exact(&mut buf));
            assert!(
                futures::poll!(read.as_mut()).is_pending(),
                "the read didn't wait for the move"
            );
            assert!(!mv.is_finished());
            ctl.move_gate.release();
            mv.await??;
            timeout(WAIT, read)
                .await
                .context("the read didn't end after the move")??;
        }
        assert!(buf == file.data, "the read returned other bytes");
        assert_eq!(handle.output_folder(), new);
        assert!(
            !old.join(STREAMED).exists(),
            "the file is still at the old place"
        );
        content.assert_at(&new, &[])?;
        Ok(())
    })
    .await
}

// A read while a rename has the storage waits for the rename, then reads the file under its
// new name.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_stream_read_waits_for_rename() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let file = content.file(STREAMED);
        let renamed = "renamed/b2.bin";
        let dir = TempDir::with_prefix("rqbit_stream_rename")?;
        let folder = dir.path().join("out");
        let ctl = Arc::new(Ctl::default());
        let gate = ctl.rename_gate.clone();
        let (session, handle) = add_complete(
            dir.path(),
            &folder,
            &content,
            InstrumentedFactory { ctl }.boxed(),
            None,
        )
        .await?;
        let mut stream = handle.clone().stream(file.index).await?;

        gate.arm();
        let rename = tokio::spawn({
            let session = session.clone();
            let handle = handle.clone();
            let index = file.index;
            async move { session.rename_file(&handle, index, renamed.into()).await }
        });
        gate.wait_entered().await?;

        let mut buf = vec![0u8; file.data.len()];
        {
            let mut read = pin!(stream.read_exact(&mut buf));
            assert!(
                futures::poll!(read.as_mut()).is_pending(),
                "the read didn't wait for the rename"
            );
            assert!(!rename.is_finished());
            gate.release();
            rename.await??;
            timeout(WAIT, read)
                .await
                .context("the read didn't end after the rename")??;
        }
        assert!(buf == file.data, "the read returned other bytes");
        assert_eq!(handle.file_path(file.index), Some(PathBuf::from(renamed)));
        assert!(
            !folder.join(STREAMED).exists(),
            "the file kept its old name"
        );
        content.assert_at(&folder, &[(file.index, renamed)])?;
        Ok(())
    })
    .await
}

// A read while a recheck runs waits for the check to end, then reads the data.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_stream_read_waits_for_recheck() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let file = content.file(STREAMED);
        let dir = TempDir::with_prefix("rqbit_stream_recheck")?;
        let folder = dir.path().join("out");
        let ctl = Arc::new(Ctl::default());
        let (session, handle) = add_complete(
            dir.path(),
            &folder,
            &content,
            InstrumentedFactory { ctl: ctl.clone() }.boxed(),
            None,
        )
        .await?;
        let mut stream = handle.clone().stream(file.index).await?;

        // Hold the check on its first read.
        ctl.read_gate.arm();
        session.force_recheck(&handle).await?;
        ctl.read_gate.wait_entered().await?;

        let mut buf = vec![0u8; file.data.len()];
        {
            let mut read = pin!(stream.read_exact(&mut buf));
            assert!(
                futures::poll!(read.as_mut()).is_pending(),
                "the read didn't wait for the recheck"
            );
            assert!(check_running(&handle), "the check isn't held");
            ctl.read_gate.release();
            timeout(WAIT, read)
                .await
                .context("the read didn't end after the recheck")??;
        }
        assert!(buf == file.data, "the read returned other bytes");
        wait_initialized(&handle).await?;
        assert!(is_live(&handle), "{:?}", state(&handle));
        assert_eq!(have_pieces(&handle)?, content.all_pieces());
        Ok(())
    })
    .await
}

// A read while a recheck runs that finds a piece of the file damaged doesn't return the
// damaged bytes: it waits for the piece to be downloaded again from a seeder, and returns the
// right ones.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_stream_read_after_recheck_waits_for_damaged_piece() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, false);
        let file = content.file(STREAMED);
        let damaged = file.offset / PIECE_LEN + 1;
        assert!(content.pieces_of(&[STREAMED]).contains(&damaged));
        let other = start_seeder(&content, None).await?;

        let dir = TempDir::with_prefix("rqbit_stream_recheck_damaged")?;
        let folder = dir.path().join("out");
        let ctl = Arc::new(Ctl::default());
        let (session, handle) = add_complete(
            dir.path(),
            &folder,
            &content,
            InstrumentedFactory { ctl: ctl.clone() }.boxed(),
            Some(vec![other.addr]),
        )
        .await?;
        let mut stream = handle.clone().stream(file.index).await?;

        content.corrupt(&folder, damaged);
        ctl.read_gate.arm();
        session.force_recheck(&handle).await?;
        ctl.read_gate.wait_entered().await?;

        let mut buf = vec![0u8; file.data.len()];
        {
            let mut read = pin!(stream.read_exact(&mut buf));
            assert!(
                futures::poll!(read.as_mut()).is_pending(),
                "the read didn't wait for the recheck"
            );
            ctl.read_gate.release();
            timeout(WAIT, read)
                .await
                .context("the read didn't end after the recheck")??;
        }
        assert!(
            buf == file.data,
            "the read returned other bytes than the file's (the damaged ones?)"
        );
        // The bytes came from the seeder: the recheck did find the piece damaged.
        let fetched = handle
            .live()
            .context("not live")?
            .stats_snapshot()
            .fetched_bytes;
        assert!(
            fetched >= PIECE_LEN as u64,
            "the damaged piece wasn't downloaded again, fetched {fetched} bytes"
        );
        wait_completed(&handle).await?;
        content.assert_at(&folder, &[])?;
        drop(other);
        Ok(())
    })
    .await
}

// A read waiting for a move fails once the torrent is deleted, instead of waiting forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_stream_read_waiting_for_move_fails_on_delete() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let file = content.file(STREAMED);
        let dir = TempDir::with_prefix("rqbit_stream_move_delete")?;
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        let ctl = Arc::new(Ctl::default());
        let (session, handle) = add_complete(
            dir.path(),
            &old,
            &content,
            InstrumentedFactory { ctl: ctl.clone() }.boxed(),
            None,
        )
        .await?;
        let mut stream = handle.clone().stream(file.index).await?;

        ctl.move_gate.arm();
        let mv = tokio::spawn({
            let session = session.clone();
            let handle = handle.clone();
            let new = new.clone();
            async move {
                session
                    .move_storage(&handle, new, MoveStorageMode::FailIfExists)
                    .await
            }
        });
        ctl.move_gate.wait_entered().await?;

        let mut buf = vec![0u8; file.data.len()];
        let read = {
            let mut read = pin!(stream.read_exact(&mut buf));
            assert!(
                futures::poll!(read.as_mut()).is_pending(),
                "the read didn't wait for the move"
            );
            let del = tokio::spawn({
                let session = session.clone();
                let id = handle.id();
                async move { session.delete(id.into(), false).await }
            });
            // The move is still held: only the delete can end the read.
            let read = timeout(WAIT, read)
                .await
                .context("the read kept waiting after the delete")?;
            ctl.move_gate.release();
            del.await??;
            read
        };
        assert!(read.is_err(), "the read of a deleted torrent succeeded");
        assert!(mv.await?.is_err(), "the move went through a delete");
        content.assert_at(&old, &[])?;
        Ok(())
    })
    .await
}
