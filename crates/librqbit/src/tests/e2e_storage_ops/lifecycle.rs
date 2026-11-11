//! Storage operations against the life of the torrent and of the session: a caller that
//! stops waiting, `Session::stop`, a crash between moving and persisting, a torrent in error
//! or waiting for its check, delete and restart during a recheck.

use std::pin::pin;

use super::*;
use crate::storage::filesystem::relocate::partial_path;

// Polls `op` until `gate` holds it, then drops it: the caller stops waiting right when the
// operation is busy on the storage.
async fn drop_at_gate(
    op: impl Future<Output = anyhow::Result<()>>,
    gate: &Arc<Gate>,
) -> anyhow::Result<()> {
    let mut op = pin!(op);
    tokio::select! {
        biased;
        entered = gate.wait_entered() => entered,
        done = &mut op => {
            done?;
            bail!("the operation finished without reaching the gate")
        }
    }
}

// Polls `op` once, which starts the task the operation runs in, checks that it waits (for
// another operation, or for a check to stop), and drops it.
async fn start_and_drop(op: impl Future<Output = anyhow::Result<()>>) -> anyhow::Result<()> {
    let mut op = pin!(op);
    if futures::poll!(op.as_mut()).is_ready() {
        bail!("the operation didn't have to wait");
    }
    Ok(())
}

// Waits until a task holds the storage operation lock of the torrent.
async fn wait_op_running(handle: &ManagedTorrent) -> anyhow::Result<()> {
    wait_for("an operation to hold the torrent", || {
        handle.storage_op.try_lock().is_err()
    })
    .await
}

// The bytes of the files of the torrent, padding excluded: what a full check reads.
fn file_bytes(content: &Content) -> u64 {
    content.files.iter().map(|f| f.data.len() as u64).sum()
}

fn session_json(db: &Path) -> anyhow::Result<String> {
    std::fs::read_to_string(db.join("session.json")).context("reading session.json")
}

fn instrumented(ctl: &Arc<Ctl>) -> AddTorrentOptions {
    AddTorrentOptions {
        storage_factory: Some(InstrumentedFactory { ctl: ctl.clone() }.boxed()),
        ..Default::default()
    }
}

// Adds `content` stored in `folder` without waiting for its check.
async fn add_unchecked(
    session: &Arc<Session>,
    content: &Content,
    folder: &Path,
    ctl: &Arc<Ctl>,
) -> anyhow::Result<ManagedTorrentHandle> {
    session
        .add_torrent(
            AddTorrent::from_bytes(content.torrent.clone()),
            Some(AddTorrentOptions {
                output_folder: Some(folder.to_str().context("utf-8")?.to_owned()),
                overwrite: true,
                ..instrumented(ctl)
            }),
        )
        .await?
        .into_handle()
        .context("into_handle")
}

// Stops `session` while a move of `handle` is held at `ctl.move_gate`, and returns what the
// move returned. Stopping cancels the move; the gate is released once it is cancelled.
async fn stop_during_move(
    session: &Arc<Session>,
    handle: &ManagedTorrentHandle,
    ctl: &Arc<Ctl>,
    mv: tokio::task::JoinHandle<anyhow::Result<()>>,
) -> anyhow::Result<anyhow::Result<()>> {
    let stop = tokio::spawn({
        let session = session.clone();
        async move { session.stop().await }
    });
    wait_for("the stop to cancel the move", || {
        handle.is_storage_op_cancelled()
    })
    .await?;
    ctl.move_gate.release();
    stop.await?;
    Ok(mv.await?)
}

fn spawn_move(
    session: &Arc<Session>,
    handle: &ManagedTorrentHandle,
    to: &Path,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    let session = session.clone();
    let handle = handle.clone();
    let to = to.to_owned();
    tokio::spawn(async move {
        session
            .move_storage(&handle, to, MoveStorageMode::FailIfExists)
            .await
    })
}

// ---------------------------------------------------------------------------------------------
// Callers that stop waiting
// ---------------------------------------------------------------------------------------------
//
// An operation runs in a task of its own: dropping the caller's future neither stops nor
// undoes it. It runs to its end, then the torrent is back in its state with its storage.

// The move's caller goes away while the files move; a rename and a recheck queued behind
// it lose their callers too. All three still happen, and the torrent works afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_dropped_move_and_queued_operations_run_to_their_end() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let dir = TempDir::with_prefix("rqbit_drop_move")?;
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        content.write_to(&old);
        let session = new_session(dir.path(), Default::default()).await?;
        let ctl = Arc::new(Ctl::default());
        let handle = add(&session, &content, &old, instrumented(&ctl)).await?;
        assert!(is_live(&handle));
        let a = content.file("a.bin").index;
        let read_before = ctl.read_bytes.load(Ordering::SeqCst);

        ctl.move_gate.arm();
        drop_at_gate(
            session.move_storage(&handle, new.clone(), MoveStorageMode::FailIfExists),
            &ctl.move_gate,
        )
        .await?;
        start_and_drop(session.rename_file(&handle, a, "r/a.bin".into())).await?;
        start_and_drop(session.force_recheck(&handle)).await?;
        assert_eq!(handle.output_folder(), old, "the move went past the gate");
        ctl.move_gate.release();

        // Each step leaves the torrent stopped or checking, so it is live only after the
        // last one.
        wait_for("the move, the rename and the recheck", || {
            handle.output_folder() == new
                && handle.file_path(a) == Some(PathBuf::from("r/a.bin"))
                && ctl.read_bytes.load(Ordering::SeqCst) - read_before >= file_bytes(&content)
                && is_live(&handle)
        })
        .await?;
        content.assert_at(&new, &[(a, "r/a.bin")])?;
        no_torrent_files_at(&content, &old)?;
        assert_eq!(have_pieces(&handle)?, content.all_pieces());

        // The storage is back in the torrent.
        session.pause(&handle).await?;
        assert!(is_paused(&handle));
        session.unpause(&handle).await?;
        assert!(is_live(&handle));
        session
            .move_storage(&handle, old.clone(), MoveStorageMode::FailIfExists)
            .await?;
        content.assert_at(&old, &[(a, "r/a.bin")])?;
        no_torrent_files_at(&content, &new)?;
        assert!(is_live(&handle));
        assert_eq!(have_pieces(&handle)?, content.all_pieces());
        Ok(())
    })
    .await
}

// The rename's caller goes away while the file is renamed, and a move queued behind it
// loses its caller while it waits for the rename. Both happen.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_dropped_rename_and_queued_move_run_to_their_end() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let dir = TempDir::with_prefix("rqbit_drop_rename")?;
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        content.write_to(&old);
        let session = new_session(dir.path(), Default::default()).await?;
        let ctl = Arc::new(Ctl::default());
        let handle = add(&session, &content, &old, instrumented(&ctl)).await?;
        let a = content.file("a.bin").index;

        ctl.rename_gate.arm();
        drop_at_gate(
            session.rename_file(&handle, a, "r/a.bin".into()),
            &ctl.rename_gate,
        )
        .await?;
        start_and_drop(session.move_storage(&handle, new.clone(), MoveStorageMode::FailIfExists))
            .await?;
        assert!(old.join("a.bin").exists(), "the rename went past the gate");
        ctl.rename_gate.release();

        // The move runs after the rename and leaves the torrent stopped until it's over.
        wait_for("the rename and the move", || {
            handle.output_folder() == new && is_live(&handle)
        })
        .await?;
        assert_eq!(handle.file_path(a), Some(PathBuf::from("r/a.bin")));
        content.assert_at(&new, &[(a, "r/a.bin")])?;
        no_torrent_files_at(&content, &old)?;
        assert!(!old.join("r").exists());
        assert_eq!(have_pieces(&handle)?, content.all_pieces());

        session.pause(&handle).await?;
        session.unpause(&handle).await?;
        assert!(is_live(&handle));
        // Back to its published path.
        session.rename_file(&handle, a, "a.bin".into()).await?;
        content.assert_at(&new, &[])?;
        assert!(is_live(&handle));
        Ok(())
    })
    .await
}

// A recheck is held on its first read. A second recheck (waiting for the first to stop) and
// a move (waiting for the second) lose their callers. The move happens and the check the
// rechecks asked for is not skipped: the damaged piece is missing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_dropped_operations_waiting_for_a_held_check() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, false);
        let dir = TempDir::with_prefix("rqbit_drop_check")?;
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        content.write_to(&old);
        let session = new_session(dir.path(), Default::default()).await?;
        let ctl = Arc::new(Ctl::default());
        let handle = add(&session, &content, &old, instrumented(&ctl)).await?;
        assert_eq!(have_pieces(&handle)?, content.all_pieces());

        // Piece 1 lies in a.bin.
        content.corrupt(&old, 1);
        ctl.read_gate.arm();
        session.force_recheck(&handle).await?;
        ctl.read_gate.wait_entered().await?;

        start_and_drop(session.force_recheck(&handle)).await?;
        // The second recheck holds the torrent while it waits for the check to stop, so the
        // move surely comes after it.
        wait_op_running(&handle).await?;
        start_and_drop(session.move_storage(&handle, new.clone(), MoveStorageMode::FailIfExists))
            .await?;
        assert_eq!(handle.output_folder(), old);
        ctl.read_gate.release();

        // The move is the last operation; it leaves the torrent stopped, or restarts the check
        // it interrupted, so the torrent is live only once everything is over.
        wait_for("the recheck and the move", || {
            handle.output_folder() == new && is_live(&handle)
        })
        .await?;
        assert_eq!(have_pieces(&handle)?, without(content.all_pieces(), &[1]));
        assert!(!handle.stats().finished);
        for f in &content.files {
            let data = std::fs::read(new.join(f.path))?;
            if f.path != "a.bin" {
                assert_eq!(data, f.data, "{}", f.path);
            }
        }
        no_torrent_files_at(&content, &old)?;

        session.pause(&handle).await?;
        session.unpause(&handle).await?;
        assert!(is_live(&handle));
        session
            .move_storage(&handle, old.clone(), MoveStorageMode::FailIfExists)
            .await?;
        assert_eq!(handle.output_folder(), old);
        assert_eq!(have_pieces(&handle)?, without(content.all_pieces(), &[1]));
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Session::stop and crashes
// ---------------------------------------------------------------------------------------------

// Stopping the session in the middle of a move undoes the move before `stop` returns; the
// store says the old folder, and a new session finds the torrent there, complete.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_stop_during_move_undoes_it() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let dir = TempDir::with_prefix("rqbit_stop_move")?;
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        let db = dir.path().join("db");
        content.write_to(&old);
        let opts = || SessionOpts {
            persistence: Some(db.clone()),
            ..Default::default()
        };
        let session = new_session(dir.path(), opts()).await?;
        let ctl = Arc::new(Ctl::default());
        let handle = add(&session, &content, &old, instrumented(&ctl)).await?;
        let info_hash = handle.info_hash();

        ctl.move_gate.arm();
        let mv = spawn_move(&session, &handle, &new);
        ctl.move_gate.wait_entered().await?;
        assert!(
            session_json(&db)?.contains("move_in_progress"),
            "the move isn't recorded before the files move"
        );
        let moved = stop_during_move(&session, &handle, &ctl, mv).await?;
        assert!(moved.is_err(), "the move went through a stop");

        assert_eq!(handle.output_folder(), old);
        content.assert_at(&old, &[])?;
        no_torrent_files_at(&content, &new)?;
        assert!(!session_json(&db)?.contains("move_in_progress"));
        drop(handle);
        drop(session);

        let session = new_session(dir.path(), opts()).await?;
        let handle = session
            .get(TorrentIdOrHash::Hash(info_hash))
            .context("torrent not restored")?;
        wait_initialized(&handle).await?;
        assert_eq!(handle.output_folder(), old);
        assert_eq!(have_pieces(&handle)?, content.all_pieces());
        content.assert_at(&old, &[])?;
        no_torrent_files_at(&content, &new)?;
        Ok(())
    })
    .await
}

// A crash after the store recorded a move and some files moved: the store as it was then,
// a.bin already at the new folder, and a half-done copy of sub/c.bin there. Loading the
// store puts everything back at the old folder, which the store names, and the torrent is
// complete there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_interrupted_move_is_undone_on_load() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let dir = TempDir::with_prefix("rqbit_crash_move")?;
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        let db = dir.path().join("db");
        let crashed_db = dir.path().join("crashed-db");
        content.write_to(&old);
        let session = new_session(
            dir.path(),
            SessionOpts {
                persistence: Some(db.clone()),
                ..Default::default()
            },
        )
        .await?;
        let ctl = Arc::new(Ctl::default());
        let handle = add(&session, &content, &old, instrumented(&ctl)).await?;
        let info_hash = handle.info_hash();

        // The store as a crash right before the first file moves leaves it.
        ctl.move_gate.arm();
        let mv = spawn_move(&session, &handle, &new);
        ctl.move_gate.wait_entered().await?;
        std::fs::create_dir_all(&crashed_db)?;
        for e in std::fs::read_dir(&db)? {
            let p = e?.path();
            std::fs::copy(&p, crashed_db.join(p.file_name().context("no file name")?))?;
        }
        assert!(session_json(&crashed_db)?.contains("move_in_progress"));
        // This session is done with; it puts the files back as it stops.
        assert!(
            stop_during_move(&session, &handle, &ctl, mv)
                .await?
                .is_err()
        );
        drop(handle);
        drop(session);
        content.assert_at(&old, &[])?;

        // The files as the crash left them.
        std::fs::create_dir_all(&new)?;
        std::fs::rename(old.join("a.bin"), new.join("a.bin"))?;
        let partial = partial_path(&new.join("sub/c.bin"))?;
        std::fs::create_dir_all(partial.parent().context("no parent")?)?;
        std::fs::write(&partial, &content.file("sub/c.bin").data[..1000])?;

        let session = new_session(
            dir.path(),
            SessionOpts {
                persistence: Some(crashed_db.clone()),
                ..Default::default()
            },
        )
        .await?;
        let handle = session
            .get(TorrentIdOrHash::Hash(info_hash))
            .context("torrent not restored")?;
        wait_initialized(&handle).await?;
        assert_eq!(handle.output_folder(), old);
        content.assert_at(&old, &[])?;
        no_torrent_files_at(&content, &new)?;
        assert!(!partial.exists(), "the half-done copy is still there");
        assert!(
            !session_json(&crashed_db)?.contains("move_in_progress"),
            "the store still remembers the move"
        );
        assert_eq!(have_pieces(&handle)?, content.all_pieces());

        // The data really is all there.
        session.force_recheck(&handle).await?;
        wait_initialized(&handle).await?;
        assert!(is_live(&handle));
        assert_eq!(have_pieces(&handle)?, content.all_pieces());
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Torrents in error or waiting for their check
// ---------------------------------------------------------------------------------------------

// A torrent in error is moved and has a file renamed. It stays in error (an operation
// doesn't resume a torrent); resuming it checks it and downloads to the new place.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_operations_on_errored_torrent() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(TRANSFER_FILES, false);
        let seed = start_seeder(&content, Some(SLOW_BPS)).await?;
        let client = start_client(&content, &seed, None, true).await?;
        client.ctl.fail_writes.store(true, Ordering::SeqCst);
        client.session.unpause(&client.handle).await?;
        wait_for("the torrent to fail", || {
            matches!(state(&client.handle), TorrentStatsState::Error)
        })
        .await?;

        let new = client.dir.path().join("moved");
        client
            .session
            .move_storage(&client.handle, new.clone(), MoveStorageMode::FailIfExists)
            .await?;
        assert_eq!(client.handle.output_folder(), new);
        assert!(matches!(state(&client.handle), TorrentStatsState::Error));
        no_torrent_files_at(&content, &client.out())?;

        let a = content.file("a.bin").index;
        client
            .session
            .rename_file(&client.handle, a, "r/a.bin".into())
            .await?;
        assert_eq!(client.handle.file_path(a), Some(PathBuf::from("r/a.bin")));
        assert!(matches!(state(&client.handle), TorrentStatsState::Error));

        client.ctl.fail_writes.store(false, Ordering::SeqCst);
        client.session.unpause(&client.handle).await?;
        wait_completed(&client.handle).await?;
        content.assert_at(&new, &[(a, "r/a.bin")])?;
        assert!(!new.join("a.bin").exists());
        no_torrent_files_at(&content, &client.out())?;
        Ok(())
    })
    .await
}

// With one check at a time, a torrent waits for its check while another torrent's check is
// held. Moving it and renaming one of its files doesn't wait for the other torrent: both
// happen at once, without hashing anything, and its check stays queued. When its turn
// comes, it checks the files at the new place.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_operations_on_torrent_waiting_for_its_check() -> anyhow::Result<()> {
    run(async {
        let first = Content::new(NESTED_FILES, false);
        let second = Content::new(SIX_PIECES, false);
        let dir = TempDir::with_prefix("rqbit_ops_queued_check")?;
        let first_dir = dir.path().join("first");
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        first.write_to(&first_dir);
        second.write_to(&old);
        let session = Session::new_with_opts(
            dir.path().to_owned(),
            SessionOptions {
                dht: None,
                persistence: None,
                listen: None,
                connect: Some(ConnectionOptions::default()),
                ipv4_only: true,
                disable_local_service_discovery: true,
                concurrent_init_limit: Some(1),
                ..Default::default()
            },
        )
        .await?;

        let first_ctl = Arc::new(Ctl::default());
        first_ctl.read_gate.arm();
        let first_handle = add_unchecked(&session, &first, &first_dir, &first_ctl).await?;
        first_ctl.read_gate.wait_entered().await?;

        let ctl = Arc::new(Ctl::default());
        let handle = add_unchecked(&session, &second, &old, &ctl).await?;
        wait_for("the check to be queued", || check_running(&handle)).await?;

        session
            .move_storage(&handle, new.clone(), MoveStorageMode::FailIfExists)
            .await?;
        assert_eq!(handle.output_folder(), new);
        second.assert_at(&new, &[])?;
        no_torrent_files_at(&second, &old)?;
        let one = second.file("one.bin").index;
        session
            .rename_file(&handle, one, "r/one.bin".into())
            .await?;
        second.assert_at(&new, &[(one, "r/one.bin")])?;
        assert_eq!(
            ctl.read_bytes.load(Ordering::SeqCst),
            0,
            "checked out of turn"
        );
        assert!(check_running(&handle), "the check isn't queued any more");
        assert!(matches!(
            state(&handle),
            TorrentStatsState::Initializing { paused: false }
        ));

        first_ctl.read_gate.release();
        wait_initialized(&first_handle).await?;
        wait_initialized(&handle).await?;
        assert!(is_live(&handle), "{:?}", state(&handle));
        assert_eq!(have_pieces(&handle)?, second.all_pieces());
        assert!(ctl.read_bytes.load(Ordering::SeqCst) >= file_bytes(&second));
        second.assert_at(&new, &[(one, "r/one.bin")])?;
        assert_eq!(have_pieces(&first_handle)?, first.all_pieces());
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Delete and restart during a recheck
// ---------------------------------------------------------------------------------------------

// Deleting a torrent with its files while its recheck is held on a read: the delete
// returns, the torrent, its files and its store entry are gone, and the check that resumes
// afterwards brings none of them back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_delete_with_files_during_recheck() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let dir = TempDir::with_prefix("rqbit_delete_recheck")?;
        let data = dir.path().join("data");
        let db = dir.path().join("db");
        content.write_to(&data);
        let session = new_session(
            dir.path(),
            SessionOpts {
                persistence: Some(db.clone()),
                ..Default::default()
            },
        )
        .await?;
        let ctl = Arc::new(Ctl::default());
        let handle = add(&session, &content, &data, instrumented(&ctl)).await?;

        ctl.read_gate.arm();
        session.force_recheck(&handle).await?;
        ctl.read_gate.wait_entered().await?;
        let check = handle
            .with_state(|s| match s {
                ManagedTorrentState::Initializing(i) => Some(i.clone()),
                _ => None,
            })
            .context("not checking")?;

        session.delete(handle.id().into(), true).await?;
        assert!(session.get(handle.id().into()).is_none());
        no_torrent_files_at(&content, &data)?;
        assert!(!data.exists(), "the output folder was left");

        ctl.read_gate.release();
        wait_for("the check to end", || !check.is_check_running()).await?;
        assert!(session.get(handle.id().into()).is_none());
        no_torrent_files_at(&content, &data)?;
        assert!(!data.exists(), "the check created files again");
        assert!(stored_bitfield(&db).is_err(), "a bitfield was stored");
        assert!(!session_json(&db)?.contains(&handle.info_hash().as_string()));
        for e in std::fs::read_dir(&db)? {
            let p = e?.path();
            assert!(
                p.file_name().is_some_and(|n| n == "session.json"),
                "{p:?} was left in the store"
            );
        }
        Ok(())
    })
    .await
}

// The session stops while a recheck is paused half-way, and starts again from its store:
// the torrent is paused, and has exactly the pieces a full check of its data finds, not the
// bitfield it had before the recheck.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_restart_during_paused_recheck() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, false);
        let dir = TempDir::with_prefix("rqbit_restart_recheck")?;
        let data = dir.path().join("data");
        let db = dir.path().join("db");
        content.write_to(&data);
        let opts = || SessionOpts {
            persistence: Some(db.clone()),
            ..Default::default()
        };
        let session = new_session(dir.path(), opts()).await?;
        let ctl = Arc::new(Ctl::default());
        let handle = add(&session, &content, &data, instrumented(&ctl)).await?;
        assert_eq!(stored_bitfield(&db)?, content.all_pieces());
        let info_hash = handle.info_hash();

        content.corrupt(&data, 3);
        ctl.read_gate.arm();
        session.force_recheck(&handle).await?;
        ctl.read_gate.wait_entered().await?;
        session.pause(&handle).await?;
        ctl.read_gate.release();
        wait_for("the check to stop", || !check_running(&handle)).await?;
        assert!(matches!(
            state(&handle),
            TorrentStatsState::Initializing { paused: true }
        ));
        assert!(
            stored_bitfield(&db).is_err(),
            "the bitfield from before the recheck is still stored"
        );
        session.stop().await;
        drop(handle);
        drop(session);

        let session = new_session(dir.path(), opts()).await?;
        let handle = session
            .get(TorrentIdOrHash::Hash(info_hash))
            .context("torrent not restored")?;
        wait_initialized(&handle).await?;
        assert!(is_paused(&handle), "{:?}", state(&handle));
        assert!(handle.is_paused());
        let expected = without(content.all_pieces(), &[3]);
        assert_eq!(have_pieces(&handle)?, expected);
        assert_eq!(stored_bitfield(&db)?, expected);
        Ok(())
    })
    .await
}
