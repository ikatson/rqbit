//! Moving files of a torrent around on disk, for [`super::FilesystemStorage`]'s
//! `move_storage` and `rename_file`. Kept apart from the storage so that the undo logic can
//! be tested on plain directories.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, bail};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::storage::{MoveStorageMode, MoveStorageOutcome, MoveStorageRequest};

// Big enough that copying isn't dominated by syscalls, small enough to check for
// cancellation often.
const COPY_BUFFER_LEN: usize = 1024 * 1024;

/// Moves the file `src` to `dst`, which must not exist: a rename when the two are on the
/// same filesystem, otherwise a copy (flushed to disk and length-checked) followed by the
/// removal of `src`. On error, `src` is where it was and `dst` doesn't exist.
pub(crate) fn move_file(src: &Path, dst: &Path, cancel: &CancellationToken) -> anyhow::Result<()> {
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            debug!(?src, ?dst, "can't rename across filesystems, copying");
            copy_then_remove(src, dst, cancel)
        }
        Err(e) => Err(e).with_context(|| format!("error renaming {src:?} to {dst:?}")),
    }
}

/// The cross-filesystem half of [`move_file`].
pub(crate) fn copy_then_remove(
    src: &Path,
    dst: &Path,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    if let Err(e) = copy_and_sync(src, dst, cancel) {
        remove_leftover(dst);
        return Err(e);
    }
    if let Err(e) = fs::remove_file(src) {
        // Two copies would leave the caller unsure which one is the file: keep the original.
        remove_leftover(dst);
        return Err(e)
            .with_context(|| format!("error removing {src:?} after copying it to {dst:?}"));
    }
    Ok(())
}

fn remove_leftover(path: &Path) {
    if let Err(e) = fs::remove_file(path)
        && e.kind() != io::ErrorKind::NotFound
    {
        warn!(?path, "error removing an incomplete copy: {e:#}");
    }
}

fn copy_and_sync(src: &Path, dst: &Path, cancel: &CancellationToken) -> anyhow::Result<()> {
    let mut from = File::open(src).with_context(|| format!("error opening {src:?}"))?;
    let from_meta = from
        .metadata()
        .with_context(|| format!("error reading metadata of {src:?}"))?;
    let mut to = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)
        .with_context(|| format!("error creating {dst:?}"))?;
    let mut buf = vec![0u8; COPY_BUFFER_LEN];
    loop {
        if cancel.is_cancelled() {
            bail!("cancelled while copying {src:?} to {dst:?}");
        }
        let read = match from.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => read,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).with_context(|| format!("error reading {src:?}")),
        };
        let chunk = buf.get(..read).context("bug: read more than the buffer")?;
        to.write_all(chunk)
            .with_context(|| format!("error writing {dst:?}"))?;
    }
    to.sync_all()
        .with_context(|| format!("error flushing {dst:?} to disk"))?;
    let copied = to
        .metadata()
        .with_context(|| format!("error reading metadata of {dst:?}"))?
        .len();
    if copied != from_meta.len() {
        bail!(
            "copy of {src:?} to {dst:?} is {copied} bytes long instead of {}",
            from_meta.len()
        );
    }
    fs::set_permissions(dst, from_meta.permissions())
        .with_context(|| format!("error copying the permissions of {src:?} to {dst:?}"))?;
    Ok(())
}

/// Creates the directories `path` needs and returns the ones that didn't exist, outermost
/// first, so that an undo can remove exactly those.
pub(crate) fn create_parents(path: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let Some(parent) = path.parent() else {
        return Ok(Vec::new());
    };
    let mut missing = Vec::new();
    let mut dir = parent;
    while !dir.as_os_str().is_empty() && fs::symlink_metadata(dir).is_err() {
        missing.push(dir.to_owned());
        match dir.parent() {
            Some(p) => dir = p,
            None => break,
        }
    }
    fs::create_dir_all(parent).with_context(|| format!("error creating directory {parent:?}"))?;
    missing.reverse();
    Ok(missing)
}

/// Removes the directories [`create_parents`] created, innermost first, as long as they are
/// empty.
pub(crate) fn remove_created_dirs(created: &[PathBuf]) {
    for dir in created.iter().rev() {
        if let Err(e) = fs::remove_dir(dir) {
            debug!(?dir, "not removing a directory created for a move: {e:#}");
        }
    }
}

/// Removes the directories between `root` and the file at `relative`, innermost first, that
/// are empty; stops at the first one that isn't. Never removes `root` itself.
pub(crate) fn remove_empty_parents(root: &Path, relative: &Path) {
    let mut dir = relative.parent();
    while let Some(d) = dir {
        if d.as_os_str().is_empty() {
            break;
        }
        let full = root.join(d);
        if let Err(e) = fs::remove_dir(&full) {
            // Not empty (the usual case), or already gone.
            debug!(dir = ?full, "not removing directory: {e:#}");
            break;
        }
        dir = d.parent();
    }
}

/// A key to compare paths by, ignoring case: some filesystems (Windows, macOS by default)
/// do, so two paths that differ only by case may name the same file.
pub(crate) fn path_key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

pub(crate) fn is_plain_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

pub(crate) fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn file_len(path: &Path) -> Option<u64> {
    fs::symlink_metadata(path)
        .ok()
        .filter(|m| m.is_file())
        .map(|m| m.len())
}

// A free name next to `path` to keep what was there aside while it's replaced.
fn aside_path(path: &Path) -> anyhow::Result<PathBuf> {
    let name = path
        .file_name()
        .with_context(|| format!("{path:?} has no file name"))?
        .to_string_lossy()
        .into_owned();
    for n in 0u32..1000 {
        let candidate = path.with_file_name(format!(".{name}.rqbit-replaced-{n}"));
        if !exists(&candidate) {
            return Ok(candidate);
        }
    }
    bail!("no free name to set {path:?} aside")
}

enum Step {
    /// The file at `src` now is at `dst`. `aside` is where what was at `dst` before went.
    Moved {
        relative: PathBuf,
        src: PathBuf,
        dst: PathBuf,
        aside: Option<PathBuf>,
    },
    /// The file already at the destination is kept; `remove_src` is the source to remove
    /// once the whole move is final.
    Adopted {
        relative: PathBuf,
        remove_src: Option<PathBuf>,
    },
}

/// Files moved by [`move_files`], to be made final with [`Self::commit`] or undone with
/// [`Self::rollback`].
pub(crate) struct MovedFiles {
    old_folder: PathBuf,
    steps: Vec<Step>,
    created_dirs: Vec<PathBuf>,
    adopted_files: Vec<usize>,
}

impl MovedFiles {
    /// Finishes the move: removes what was replaced, the sources of adopted files of equal
    /// length, and the source directories that became empty.
    pub(crate) fn commit(self) -> MoveStorageOutcome {
        for step in &self.steps {
            let (relative, to_remove) = match step {
                Step::Moved {
                    relative, aside, ..
                } => (relative, aside),
                Step::Adopted {
                    relative,
                    remove_src,
                } => (relative, remove_src),
            };
            if let Some(path) = to_remove
                && let Err(e) = fs::remove_file(path)
            {
                warn!(
                    ?path,
                    "error removing a file the move made redundant: {e:#}"
                );
            }
            remove_empty_parents(&self.old_folder, relative);
        }
        MoveStorageOutcome {
            adopted_files: self.adopted_files,
        }
    }

    /// Undoes the move. Returns a description of every file the undo couldn't put back.
    pub(crate) fn rollback(self) -> Result<(), String> {
        // The undo must finish even if the move was cancelled.
        let never = CancellationToken::new();
        let mut failures = Vec::new();
        for step in self.steps.into_iter().rev() {
            let Step::Moved {
                src, dst, aside, ..
            } = step
            else {
                continue;
            };
            if let Err(e) = move_file(&dst, &src, &never) {
                failures.push(format!("{src:?} is at {dst:?} ({e:#})"));
                if let Some(aside) = aside {
                    failures.push(format!("the file that was at {dst:?} is at {aside:?}"));
                }
                continue;
            }
            if let Some(aside) = aside
                && let Err(e) = fs::rename(&aside, &dst)
            {
                failures.push(format!(
                    "the file that was at {dst:?} is at {aside:?} ({e:#})"
                ));
            }
        }
        remove_created_dirs(&self.created_dirs);
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }
}

fn with_rollback(moved: MovedFiles, error: anyhow::Error) -> anyhow::Error {
    match moved.rollback() {
        Ok(()) => error,
        Err(left) => error.context(format!("and undoing the move failed: {left}")),
    }
}

/// Moves the files of `request` from `old_folder` to `request.new_output_folder`, following
/// `request.mode` (see [`MoveStorageMode`]). Everything that can be checked is checked before
/// anything is touched; on error everything moved so far is moved back.
pub(crate) fn move_files(
    old_folder: &Path,
    request: &MoveStorageRequest<'_>,
) -> anyhow::Result<MovedFiles> {
    let new_folder = request.new_output_folder;
    if let Ok(meta) = fs::metadata(new_folder)
        && !meta.is_dir()
    {
        bail!("{new_folder:?} exists and isn't a directory");
    }

    let files = request
        .files
        .iter()
        .enumerate()
        .filter_map(|(idx, rel)| Some((idx, rel.as_deref()?)))
        .collect::<Vec<_>>();
    let sources = files
        .iter()
        .map(|(_, rel)| path_key(&old_folder.join(rel)))
        .collect::<std::collections::HashSet<_>>();
    for (_, rel) in &files {
        if !is_plain_relative(rel) {
            bail!("{rel:?} isn't a relative path inside the output folder");
        }
        let dst = new_folder.join(rel);
        if sources.contains(&path_key(&dst)) {
            bail!(
                "{dst:?} is where a file of the torrent is now; moving between {old_folder:?} and \
                 {new_folder:?} would overwrite it"
            );
        }
        if let Ok(meta) = fs::symlink_metadata(&dst) {
            if meta.is_dir() {
                bail!("{dst:?} exists and is a directory");
            }
            if request.mode == MoveStorageMode::FailIfExists {
                bail!("{dst:?} already exists");
            }
        }
    }

    let mut moved = MovedFiles {
        old_folder: old_folder.to_owned(),
        steps: Vec::new(),
        created_dirs: Vec::new(),
        adopted_files: Vec::new(),
    };
    for (idx, rel) in files {
        if let Err(e) = move_one(&mut moved, idx, rel, new_folder, request) {
            return Err(with_rollback(moved, e));
        }
    }
    Ok(moved)
}

fn move_one(
    moved: &mut MovedFiles,
    idx: usize,
    rel: &Path,
    new_folder: &Path,
    request: &MoveStorageRequest<'_>,
) -> anyhow::Result<()> {
    if request.cancel.is_cancelled() {
        bail!("the move was cancelled");
    }
    let src = moved.old_folder.join(rel);
    let dst = new_folder.join(rel);
    let src_len = file_len(&src);
    let dst_len = file_len(&dst);

    if exists(&dst) {
        let adopt = match request.mode {
            MoveStorageMode::FailIfExists => bail!("{dst:?} already exists"),
            MoveStorageMode::KeepExisting => true,
            // With nothing to replace it with, what's there stays and is used.
            MoveStorageMode::Overwrite => src_len.is_none(),
        };
        if adopt {
            moved.adopted_files.push(idx);
            moved.steps.push(Step::Adopted {
                relative: rel.to_owned(),
                remove_src: (src_len.is_some() && src_len == dst_len).then_some(src),
            });
            return Ok(());
        }
        let aside = aside_path(&dst)?;
        fs::rename(&dst, &aside)
            .with_context(|| format!("error setting {dst:?} aside to {aside:?}"))?;
        if let Err(e) = move_file(&src, &dst, request.cancel) {
            if let Err(re) = fs::rename(&aside, &dst) {
                return Err(e.context(format!(
                    "and the file that was at {dst:?} is left at {aside:?} ({re:#})"
                )));
            }
            return Err(e);
        }
        moved.steps.push(Step::Moved {
            relative: rel.to_owned(),
            src,
            dst,
            aside: Some(aside),
        });
        return Ok(());
    }

    if src_len.is_none() {
        // Never created (e.g. a file outside the selection): nothing to move.
        return Ok(());
    }
    let created = create_parents(&dst)?;
    moved.created_dirs.extend(created);
    move_file(&src, &dst, request.cancel)?;
    moved.steps.push(Step::Moved {
        relative: rel.to_owned(),
        src,
        dst,
        aside: None,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use tokio_util::sync::CancellationToken;

    use super::{copy_then_remove, create_parents, move_files, remove_empty_parents};
    use crate::storage::{MoveStorageMode, MoveStorageRequest};

    fn write(path: &Path, content: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn read(path: &Path) -> Vec<u8> {
        std::fs::read(path).unwrap()
    }

    #[test]
    fn test_copy_then_remove_moves_the_content() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a");
        let dst = dir.path().join("b");
        // Bigger than one copy buffer, so the loop runs more than once.
        let content = (0..=250u8)
            .cycle()
            .take(super::COPY_BUFFER_LEN * 2 + 17)
            .collect::<Vec<_>>();
        write(&src, &content);
        copy_then_remove(&src, &dst, &CancellationToken::new()).unwrap();
        assert!(!src.exists());
        assert_eq!(read(&dst), content);
    }

    #[test]
    fn test_copy_then_remove_cancelled_leaves_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a");
        let dst = dir.path().join("b");
        write(&src, b"data");
        let cancel = CancellationToken::new();
        cancel.cancel();
        copy_then_remove(&src, &dst, &cancel).unwrap_err();
        assert_eq!(read(&src), b"data");
        assert!(!dst.exists());
    }

    #[test]
    fn test_create_parents_reports_what_it_created() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("a")).unwrap();
        let created = create_parents(&dir.path().join("a/b/c/file")).unwrap();
        assert_eq!(
            created,
            vec![dir.path().join("a/b"), dir.path().join("a/b/c")]
        );
    }

    #[test]
    fn test_remove_empty_parents_stops_at_a_non_empty_dir_and_the_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b/c")).unwrap();
        write(&dir.path().join("a/keep"), b"x");
        remove_empty_parents(dir.path(), Path::new("a/b/c/gone"));
        assert!(!dir.path().join("a/b").exists());
        assert!(dir.path().join("a/keep").exists());

        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("x")).unwrap();
        remove_empty_parents(&root, Path::new("x/gone"));
        assert!(!root.join("x").exists());
        assert!(root.exists(), "the root must stay");
    }

    fn request<'a>(
        new: &'a Path,
        files: &'a [Option<PathBuf>],
        mode: MoveStorageMode,
        cancel: &'a CancellationToken,
    ) -> MoveStorageRequest<'a> {
        MoveStorageRequest {
            new_output_folder: new,
            files,
            mode,
            cancel,
        }
    }

    #[test]
    fn test_move_files_commit_and_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        write(&old.join("d/one"), b"one");
        write(&old.join("two"), b"two");
        let files = vec![
            Some(PathBuf::from("d/one")),
            None,
            Some(PathBuf::from("two")),
            Some(PathBuf::from("never/created")),
        ];
        let cancel = CancellationToken::new();

        let moved = move_files(
            &old,
            &request(&new, &files, MoveStorageMode::FailIfExists, &cancel),
        )
        .unwrap();
        assert_eq!(read(&new.join("d/one")), b"one");
        moved.rollback().unwrap();
        assert_eq!(read(&old.join("d/one")), b"one");
        assert_eq!(read(&old.join("two")), b"two");
        assert!(!new.join("d").exists(), "created directories are removed");

        let moved = move_files(
            &old,
            &request(&new, &files, MoveStorageMode::FailIfExists, &cancel),
        )
        .unwrap();
        let outcome = moved.commit();
        assert!(outcome.adopted_files.is_empty());
        assert_eq!(read(&new.join("d/one")), b"one");
        assert_eq!(read(&new.join("two")), b"two");
        assert!(
            !old.join("d").exists(),
            "emptied source directories are removed"
        );
        assert!(old.exists(), "the old output folder itself stays");
        assert!(!new.join("never").exists());
    }

    #[test]
    fn test_move_files_modes_with_existing_destination() {
        let cancel = CancellationToken::new();
        let files = vec![Some(PathBuf::from("same")), Some(PathBuf::from("diff"))];
        let setup = || {
            let dir = tempfile::tempdir().unwrap();
            let old = dir.path().join("old");
            let new = dir.path().join("new");
            write(&old.join("same"), b"ours");
            write(&old.join("diff"), b"ours");
            write(&new.join("same"), b"THEM");
            write(&new.join("diff"), b"theirs-longer");
            (dir, old, new)
        };

        let (_dir, old, new) = setup();
        let err = move_files(
            &old,
            &request(&new, &files, MoveStorageMode::FailIfExists, &cancel),
        )
        .err()
        .unwrap();
        assert!(format!("{err:#}").contains("already exists"), "{err:#}");
        assert_eq!(read(&old.join("same")), b"ours");

        let (_dir, old, new) = setup();
        let outcome = move_files(
            &old,
            &request(&new, &files, MoveStorageMode::Overwrite, &cancel),
        )
        .unwrap()
        .commit();
        assert!(outcome.adopted_files.is_empty());
        assert_eq!(read(&new.join("same")), b"ours");
        assert_eq!(read(&new.join("diff")), b"ours");
        assert!(!old.join("same").exists());
        assert_eq!(std::fs::read_dir(&new).unwrap().count(), 2, "no leftovers");

        let (_dir, old, new) = setup();
        let outcome = move_files(
            &old,
            &request(&new, &files, MoveStorageMode::KeepExisting, &cancel),
        )
        .unwrap()
        .commit();
        assert_eq!(outcome.adopted_files, vec![0, 1]);
        assert_eq!(read(&new.join("same")), b"THEM");
        assert_eq!(read(&new.join("diff")), b"theirs-longer");
        assert!(!old.join("same").exists(), "same length: source removed");
        assert_eq!(
            read(&old.join("diff")),
            b"ours",
            "other length: source kept"
        );
    }

    #[test]
    fn test_move_files_overwrite_rollback_restores_the_replaced_file() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        write(&old.join("a"), b"ours");
        write(&new.join("a"), b"theirs");
        let files = vec![Some(PathBuf::from("a"))];
        let cancel = CancellationToken::new();
        let moved = move_files(
            &old,
            &request(&new, &files, MoveStorageMode::Overwrite, &cancel),
        )
        .unwrap();
        assert_eq!(read(&new.join("a")), b"ours");
        moved.rollback().unwrap();
        assert_eq!(read(&old.join("a")), b"ours");
        assert_eq!(read(&new.join("a")), b"theirs");
        assert_eq!(std::fs::read_dir(&new).unwrap().count(), 1, "no leftovers");
    }

    #[test]
    fn test_move_files_failure_part_way_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        write(&old.join("a"), b"a");
        write(&old.join("b/c"), b"c");
        // "b" is a file at the destination, so "b/c" can't get a directory there.
        write(&new.join("b"), b"in the way");
        let files = vec![Some(PathBuf::from("a")), Some(PathBuf::from("b/c"))];
        let cancel = CancellationToken::new();
        move_files(
            &old,
            &request(&new, &files, MoveStorageMode::FailIfExists, &cancel),
        )
        .err()
        .unwrap();
        assert_eq!(read(&old.join("a")), b"a");
        assert_eq!(read(&old.join("b/c")), b"c");
        assert!(!new.join("a").exists());
        assert_eq!(read(&new.join("b")), b"in the way");
    }

    #[test]
    fn test_move_files_cancelled_moves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        write(&old.join("a"), b"a");
        let files = vec![Some(PathBuf::from("a"))];
        let cancel = CancellationToken::new();
        cancel.cancel();
        move_files(
            &old,
            &request(&new, &files, MoveStorageMode::FailIfExists, &cancel),
        )
        .err()
        .unwrap();
        assert_eq!(read(&old.join("a")), b"a");
    }

    #[test]
    fn test_move_files_rejects_overlapping_folders_that_would_collide() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("x");
        write(&old.join("x/f"), b"1");
        write(&old.join("f"), b"2");
        // Moving to the parent: "x/f" would land on "f"'s current place.
        let files = vec![Some(PathBuf::from("x/f")), Some(PathBuf::from("f"))];
        let cancel = CancellationToken::new();
        let err = move_files(
            &old,
            &request(dir.path(), &files, MoveStorageMode::Overwrite, &cancel),
        )
        .err()
        .unwrap();
        assert!(format!("{err:#}").contains("would overwrite"), "{err:#}");
        assert_eq!(read(&old.join("x/f")), b"1");
        assert_eq!(read(&old.join("f")), b"2");
    }

    #[test]
    fn test_move_files_rejects_escaping_paths() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![Some(PathBuf::from("../escape"))];
        let cancel = CancellationToken::new();
        move_files(
            &dir.path().join("old"),
            &request(
                &dir.path().join("new"),
                &files,
                MoveStorageMode::Overwrite,
                &cancel,
            ),
        )
        .err()
        .unwrap();
    }
}
