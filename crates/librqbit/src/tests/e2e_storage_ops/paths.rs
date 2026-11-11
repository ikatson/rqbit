//! Moves and renames against the paths a filesystem really has: other spellings of the
//! same folder (symlinks, `..`, relative paths, case), destinations that already hold other
//! bytes, files deleted under a loaded torrent, and names that collide with files that are
//! not part of the torrent.

use super::*;

const MODES: [MoveStorageMode; 3] = [
    MoveStorageMode::FailIfExists,
    MoveStorageMode::Overwrite,
    MoveStorageMode::KeepExisting,
];

// Whether `dir` is on a filesystem that ignores case: a file created there can be found
// under its name in another case.
fn is_case_insensitive(dir: &Path) -> anyhow::Result<bool> {
    let probe = dir.join("case-probe");
    std::fs::write(&probe, b"")?;
    let found = std::fs::metadata(dir.join("CASE-PROBE")).is_ok();
    std::fs::remove_file(&probe)?;
    Ok(found)
}

// The pieces that hold any byte of the given files. With padding, the last piece of a file
// holds only its bytes and zeros, so it's lost with the file too.
fn pieces_touching(content: &Content, paths: &[&str]) -> Vec<usize> {
    let mut out = Vec::new();
    for f in content.files.iter().filter(|f| paths.contains(&f.path)) {
        out.extend(f.offset / PIECE_LEN..(f.offset + f.data.len()).div_ceil(PIECE_LEN));
    }
    out.sort_unstable();
    out.dedup();
    out
}

// `target` as a path relative to the current directory, which is how a relative output
// folder is resolved. `None` when there's no such path (another drive on Windows).
fn relative_to_cwd(target: &Path) -> anyhow::Result<Option<PathBuf>> {
    let cwd = std::fs::canonicalize(std::env::current_dir()?)?;
    let target = std::fs::canonicalize(target)?;
    let common = cwd
        .components()
        .zip(target.components())
        .take_while(|(a, b)| a == b)
        .count();
    // Both start with the root (and on Windows the drive): nothing in common, no relative
    // path between them.
    if common == 0 {
        return Ok(None);
    }
    let mut rel = PathBuf::new();
    for _ in cwd.components().skip(common) {
        rel.push("..");
    }
    for c in target.components().skip(common) {
        rel.push(c);
    }
    Ok(Some(rel))
}

// Moves a seeding torrent in `<tmp>/old` to `alias(<tmp>, <tmp>/old)`, another name of the
// same folder, in every mode. Each time: nothing moves or gets hashed, the torrent keeps its
// folder, all its files with their bytes and all its pieces, a recheck verifies them all, and
// another session downloads the whole content from it. `alias` returns `None` when this
// system has no such name for the folder; the case is then skipped with a note.
async fn move_to_alias_is_a_noop(
    spelling: &str,
    alias: impl Fn(&Path, &Path) -> anyhow::Result<Option<PathBuf>>,
) -> anyhow::Result<()> {
    let content = Content::new(NESTED_FILES, true);
    for mode in MODES {
        let what = format!("{spelling}, {mode:?}");
        let dir = TempDir::with_prefix("rqbit_move_alias")?;
        let old = dir.path().join("old");
        content.write_to(&old);
        let Some(alias) = alias(dir.path(), &old)? else {
            println!("skipped: no {spelling} name for {old:?} on this system");
            return Ok(());
        };
        assert_ne!(alias, old, "{what}: not another spelling");

        let session = new_session(
            dir.path(),
            SessionOpts {
                listen: true,
                ..Default::default()
            },
        )
        .await?;
        let ctl = Arc::new(Ctl::default());
        let handle = add(
            &session,
            &content,
            &old,
            AddTorrentOptions {
                storage_factory: Some(InstrumentedFactory { ctl: ctl.clone() }.boxed()),
                ..Default::default()
            },
        )
        .await?;
        assert!(is_live(&handle), "{what}");
        let read_before = ctl.read_bytes.load(Ordering::SeqCst);

        session
            .move_storage(&handle, alias.clone(), mode)
            .await
            .with_context(|| format!("{what}: moving to {alias:?}"))?;
        assert_eq!(handle.output_folder(), old, "{what}");
        content.assert_at(&old, &[]).context(what.clone())?;
        assert!(is_live(&handle), "{what}: {:?}", state(&handle));
        assert!(handle.stats().finished, "{what}");
        assert_eq!(have_pieces(&handle)?, content.all_pieces(), "{what}");
        assert_eq!(
            ctl.read_bytes.load(Ordering::SeqCst),
            read_before,
            "{what}: the no-op hashed"
        );

        session.force_recheck(&handle).await?;
        wait_initialized(&handle).await?;
        assert_eq!(have_pieces(&handle)?, content.all_pieces(), "{what}");
        content.assert_at(&old, &[]).context(what.clone())?;

        let seed = Seeder {
            addr: session.listen_addr().context("no listen addr")?,
            session,
            handle,
            dir,
        };
        let client = start_client(&content, &seed, None, false).await?;
        wait_completed(&client.handle).await.context(what.clone())?;
        content.assert_at(&client.out(), &[]).context(what)?;
    }
    Ok(())
}

// Through a symbolic link to the folder.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_move_storage_to_symlink_of_current_folder() -> anyhow::Result<()> {
    run(move_to_alias_is_a_noop("symlink", |dir, old| {
        let link = dir.join("link");
        std::os::unix::fs::symlink(old, &link)?;
        Ok(Some(link))
    }))
    .await
}

// Through a `..` component: `old/sub/..`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_move_storage_to_dotdot_of_current_folder() -> anyhow::Result<()> {
    run(move_to_alias_is_a_noop("`..`", |_, old| {
        Ok(Some(old.join("sub").join("..")))
    }))
    .await
}

// As a path relative to the current directory (relative output folders are resolved
// against it, as when adding a torrent).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_move_storage_to_relative_current_folder() -> anyhow::Result<()> {
    run(move_to_alias_is_a_noop("relative", |_, old| {
        let rel = relative_to_cwd(old)?;
        if let Some(rel) = &rel {
            assert!(rel.is_relative());
        }
        Ok(rel)
    }))
    .await
}

// In another case, where the filesystem ignores case.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_move_storage_to_current_folder_in_other_case() -> anyhow::Result<()> {
    run(move_to_alias_is_a_noop("other-case", |dir, _| {
        if !is_case_insensitive(dir)? {
            return Ok(None);
        }
        Ok(Some(dir.join("OLD")))
    }))
    .await
}

// KeepExisting with a destination file of the right length but other bytes: the destination
// file is adopted and its pieces are found missing by the recheck, the source file stays
// (its bytes aren't anywhere else), and every other file moves.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_move_storage_keep_existing_adopts_same_length_other_bytes() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let b = content.file("sub/deeper/b.bin");
        let dir = TempDir::with_prefix("rqbit_move_keep_existing")?;
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        content.write_to(&old);
        let foreign: Vec<u8> = b.data.iter().map(|x| !x).collect();
        std::fs::create_dir_all(new.join("sub/deeper"))?;
        std::fs::write(new.join(b.path), &foreign)?;

        let session = new_session(dir.path(), Default::default()).await?;
        let ctl = Arc::new(Ctl::default());
        let handle = add(
            &session,
            &content,
            &old,
            AddTorrentOptions {
                storage_factory: Some(InstrumentedFactory { ctl: ctl.clone() }.boxed()),
                ..Default::default()
            },
        )
        .await?;
        let read_before = ctl.read_bytes.load(Ordering::SeqCst);

        session
            .move_storage(&handle, new.clone(), MoveStorageMode::KeepExisting)
            .await?;
        wait_initialized(&handle).await?;
        assert_eq!(handle.output_folder(), new);
        assert!(is_live(&handle), "{:?}", state(&handle));
        assert!(
            ctl.read_bytes.load(Ordering::SeqCst) > read_before,
            "the adopted file wasn't rechecked"
        );
        assert_eq!(
            have_pieces(&handle)?,
            without(content.all_pieces(), &pieces_touching(&content, &[b.path]))
        );
        assert!(!handle.stats().finished);
        assert_eq!(
            std::fs::read(new.join(b.path))?,
            foreign,
            "destination adopted"
        );
        assert_eq!(std::fs::read(old.join(b.path))?, b.data, "source kept");
        for f in content.files.iter().filter(|f| f.path != b.path) {
            assert_eq!(std::fs::read(new.join(f.path))?, f.data, "{}", f.path);
            assert!(
                !old.join(f.path).exists(),
                "{} left at the old place",
                f.path
            );
        }
        Ok(())
    })
    .await
}

// Files deleted on disk while the torrent is loaded (a file, then a whole directory): the move
// succeeds, moves the rest with their bytes, and the missing files are created again at the
// new place (with their directories); a recheck finds exactly their pieces missing.
// Unix only: Windows may refuse to delete files a process has open.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_move_storage_with_deleted_files() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        for (deleted, gone) in [
            ("a.bin", &["a.bin"][..]),
            ("sub", &["sub/deeper/b.bin", "sub/c.bin"][..]),
        ] {
            let dir = TempDir::with_prefix("rqbit_move_deleted")?;
            let old = dir.path().join("old");
            let new = dir.path().join("new");
            content.write_to(&old);
            let session = new_session(dir.path(), Default::default()).await?;
            let handle = add(&session, &content, &old, Default::default()).await?;
            assert!(is_live(&handle));

            let deleted_path = old.join(deleted);
            if deleted_path.is_dir() {
                std::fs::remove_dir_all(&deleted_path)?;
            } else {
                std::fs::remove_file(&deleted_path)?;
            }

            session
                .move_storage(&handle, new.clone(), MoveStorageMode::FailIfExists)
                .await
                .with_context(|| format!("{deleted} deleted"))?;
            assert_eq!(handle.output_folder(), new, "{deleted}");
            assert!(is_live(&handle), "{deleted}: {:?}", state(&handle));
            for f in &content.files {
                assert!(
                    !old.join(f.path).exists(),
                    "{deleted}: {} at the old place",
                    f.path
                );
                let at = new.join(f.path);
                if gone.contains(&f.path) {
                    assert!(at.is_file(), "{deleted}: {at:?} wasn't created again");
                } else {
                    assert_eq!(std::fs::read(&at)?, f.data, "{deleted}: {at:?}");
                }
            }

            session.force_recheck(&handle).await?;
            wait_initialized(&handle).await?;
            assert_eq!(
                have_pieces(&handle)?,
                without(content.all_pieces(), &pieces_touching(&content, gone)),
                "{deleted}"
            );
            for f in content.files.iter().filter(|f| !gone.contains(&f.path)) {
                assert_eq!(std::fs::read(new.join(f.path))?, f.data, "{deleted}");
            }
        }
        Ok(())
    })
    .await
}

// On a case-sensitive filesystem, `a.bin` -> `A.bin` where an `A.bin` that isn't part of the
// torrent exists: refused, and both files keep their bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_rename_case_only_onto_foreign_file_case_sensitive() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let dir = TempDir::with_prefix("rqbit_rename_case_foreign")?;
        if is_case_insensitive(dir.path())? {
            println!(
                "skipped: {:?} is on a case-insensitive filesystem",
                dir.path()
            );
            return Ok(());
        }
        content.write_to(dir.path());
        let foreign = b"not part of the torrent".to_vec();
        std::fs::write(dir.path().join("A.bin"), &foreign)?;
        let session = new_session(dir.path(), Default::default()).await?;
        let handle = add(&session, &content, dir.path(), Default::default()).await?;
        let a = content.file("a.bin").index;

        let err = session
            .rename_file(&handle, a, "A.bin".into())
            .await
            .err()
            .context("renaming onto a foreign file was accepted")?;
        assert!(format!("{err:#}").contains("already exists"), "{err:#}");
        assert_eq!(std::fs::read(dir.path().join("A.bin"))?, foreign);
        content.assert_at(dir.path(), &[])?;
        assert_eq!(handle.file_path(a), Some(PathBuf::from("a.bin")));
        assert!(handle.shared().renamed_files().is_empty());
        assert!(is_live(&handle));
        assert_eq!(have_pieces(&handle)?, content.all_pieces());
        Ok(())
    })
    .await
}

// On a case-insensitive filesystem, `a.bin` -> `A.bin` renames the file: its name on disk
// takes the new case, its bytes stay, and it verifies under the new name.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_rename_case_only_case_insensitive() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let dir = TempDir::with_prefix("rqbit_rename_case")?;
        if !is_case_insensitive(dir.path())? {
            println!(
                "skipped: {:?} is on a case-sensitive filesystem",
                dir.path()
            );
            return Ok(());
        }
        content.write_to(dir.path());
        let session = new_session(dir.path(), Default::default()).await?;
        let handle = add(&session, &content, dir.path(), Default::default()).await?;
        let a = content.file("a.bin").index;

        session.rename_file(&handle, a, "A.bin".into()).await?;
        assert_eq!(handle.file_path(a), Some(PathBuf::from("A.bin")));
        let names = std::fs::read_dir(dir.path())?
            .map(|e| Ok(e?.file_name().to_string_lossy().into_owned()))
            .collect::<anyhow::Result<Vec<_>>>()?;
        assert!(names.iter().any(|n| n == "A.bin"), "{names:?}");
        assert!(!names.iter().any(|n| n == "a.bin"), "{names:?}");
        content.assert_at(dir.path(), &[(a, "A.bin")])?;

        session.force_recheck(&handle).await?;
        wait_initialized(&handle).await?;
        assert_eq!(have_pieces(&handle)?, content.all_pieces());
        Ok(())
    })
    .await
}

// Renaming onto a file that isn't part of the torrent (also in another case, where the
// filesystem ignores case) is refused with a clear error; neither file changes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_rename_onto_foreign_file_is_rejected() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(NESTED_FILES, true);
        let dir = TempDir::with_prefix("rqbit_rename_foreign")?;
        content.write_to(dir.path());
        let foreign = b"someone else's notes".to_vec();
        std::fs::create_dir_all(dir.path().join("notes"))?;
        std::fs::write(dir.path().join("notes/foreign.txt"), &foreign)?;
        let session = new_session(dir.path(), Default::default()).await?;
        let handle = add(&session, &content, dir.path(), Default::default()).await?;
        let a = content.file("a.bin").index;
        let c = content.file("sub/c.bin").index;

        let mut targets = vec![(a, "notes/foreign.txt"), (c, "notes/foreign.txt")];
        if is_case_insensitive(dir.path())? {
            targets.push((a, "Notes/FOREIGN.txt"));
        } else {
            println!("other-case target skipped: the filesystem is case-sensitive");
        }
        for (idx, to) in targets {
            let err = session
                .rename_file(&handle, idx, to.into())
                .await
                .err()
                .with_context(|| format!("renaming file {idx} to {to:?} was accepted"))?;
            assert!(
                format!("{err:#}").contains("already exists"),
                "{to}: unclear error: {err:#}"
            );
            assert_eq!(
                std::fs::read(dir.path().join("notes/foreign.txt"))?,
                foreign
            );
            content.assert_at(dir.path(), &[])?;
            assert_eq!(
                handle.file_path(idx),
                Some(PathBuf::from(
                    content.files.iter().find(|f| f.index == idx).unwrap().path
                ))
            );
            assert!(handle.shared().renamed_files().is_empty());
            assert!(is_live(&handle), "{:?}", state(&handle));
        }
        assert_eq!(have_pieces(&handle)?, content.all_pieces());
        Ok(())
    })
    .await
}
