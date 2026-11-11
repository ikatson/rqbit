//! Moving files of a torrent around on disk, for [`super::FilesystemStorage`]'s
//! `move_storage` and `rename_file`. Kept apart from the storage so that the undo logic can
//! be tested on plain directories.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, Write},
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
/// removal of `src`. A symbolic link is moved as a link to the same file. On error, `src` is
/// where it was and `dst` doesn't exist.
pub(crate) fn move_file(src: &Path, dst: &Path, cancel: &CancellationToken) -> anyhow::Result<()> {
    if fs::symlink_metadata(src).is_ok_and(|m| m.file_type().is_symlink()) {
        return move_symlink(src, dst);
    }
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            debug!(?src, ?dst, "can't rename across filesystems, copying");
            copy_then_remove(src, dst, cancel)
        }
        Err(e) => Err(e).with_context(|| format!("error renaming {src:?} to {dst:?}")),
    }
}

// A link with a relative target would point elsewhere from another directory (and a rename
// can't cross filesystems), so the link is made again at `dst`, pointing where `src` did.
fn move_symlink(src: &Path, dst: &Path) -> anyhow::Result<()> {
    let target = fs::read_link(src).with_context(|| format!("error reading the link {src:?}"))?;
    let target = match src.parent() {
        Some(dir) if target.is_relative() => dir.join(target),
        _ => target,
    };
    make_symlink(&target, dst)
        .with_context(|| format!("error making a link to {target:?} at {dst:?}"))?;
    if let Err(e) = fs::remove_file(src) {
        remove_leftover(dst);
        return Err(e).with_context(|| format!("error removing the link {src:?}"));
    }
    Ok(())
}

#[cfg(unix)]
fn make_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn make_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[cfg(not(any(unix, windows)))]
fn make_symlink(_target: &Path, _link: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symbolic links aren't supported on this platform",
    ))
}

/// The cross-filesystem half of [`move_file`]. The copy is made under a temporary name next
/// to `dst` ([`partial_path`]) and gets its name only once complete and flushed, so an
/// interrupted copy (even by a crash) never looks like the file.
pub(crate) fn copy_then_remove(
    src: &Path,
    dst: &Path,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    if exists(dst) {
        bail!("{dst:?} already exists");
    }
    let partial = partial_path(dst)?;
    // What a copy interrupted by a crash left.
    remove_leftover(&partial);
    // Created here, so that only a copy made by this call is ever removed below.
    let to = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial)
        .with_context(|| format!("error creating {partial:?}"))?;
    let copied = copy_and_sync(src, to, &partial, cancel).and_then(|()| publish(&partial, dst));
    if let Err(e) = copied {
        remove_leftover(&partial);
        return Err(e);
    }
    sync_parent(dst);
    if let Err(e) = fs::remove_file(src) {
        // Two copies would leave the caller unsure which one is the file: keep the original.
        remove_leftover(dst);
        return Err(e)
            .with_context(|| format!("error removing {src:?} after copying it to {dst:?}"));
    }
    Ok(())
}

/// The temporary name a copy to `path` is made under.
pub(crate) fn partial_path(path: &Path) -> anyhow::Result<PathBuf> {
    let name = path
        .file_name()
        .with_context(|| format!("{path:?} has no file name"))?
        .to_string_lossy();
    Ok(path.with_file_name(format!(".{name}.rqbit-partial")))
}

// Gives the finished copy at `partial` the name `dst`, without replacing a file that appeared
// there meanwhile.
fn publish(partial: &Path, dst: &Path) -> anyhow::Result<()> {
    match fs::hard_link(partial, dst) {
        Ok(()) => {
            if let Err(e) = fs::remove_file(partial) {
                warn!(
                    ?partial,
                    "error removing the temporary name of a copy: {e:#}"
                );
            }
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => bail!("{dst:?} already exists"),
        // A filesystem without hard links (FAT, exFAT): a rename, which would replace a file
        // silently, after a last look.
        Err(_) => {
            if exists(dst) {
                bail!("{dst:?} already exists");
            }
            fs::rename(partial, dst)
                .with_context(|| format!("error renaming {partial:?} to {dst:?}"))
        }
    }
}

fn remove_leftover(path: &Path) {
    if let Err(e) = fs::remove_file(path)
        && e.kind() != io::ErrorKind::NotFound
    {
        warn!(?path, "error removing an incomplete copy: {e:#}");
    }
}

fn copy_and_sync(
    src: &Path,
    mut to: File,
    dst: &Path,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    let mut from = File::open(src).with_context(|| format!("error opening {src:?}"))?;
    let from_meta = from
        .metadata()
        .with_context(|| format!("error reading metadata of {src:?}"))?;
    let mut buf = vec![0u8; COPY_BUFFER_LEN];
    let mut total = 0u64;
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
        let len = u64::try_from(read).context("bug: buffer longer than u64")?;
        // Torrent files are preallocated and filled as pieces come: skipping zeros keeps the
        // copy as sparse as the source on filesystems that can do it.
        if chunk.iter().all(|b| *b == 0) {
            to.seek(io::SeekFrom::Current(
                i64::try_from(len).context("chunk too long")?,
            ))
            .with_context(|| format!("error seeking in {dst:?}"))?;
        } else {
            to.write_all(chunk)
                .with_context(|| format!("error writing {dst:?}"))?;
        }
        total += len;
    }
    // Gives the file the zeros it ends with, if it does.
    to.set_len(total)
        .with_context(|| format!("error setting the length of {dst:?}"))?;
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
    // The data is what matters; a filesystem that can't take the permissions (FAT, some
    // network shares) still holds a good copy.
    if let Err(e) = fs::set_permissions(dst, from_meta.permissions()) {
        warn!(?src, ?dst, "error copying the permissions: {e:#}");
    }
    Ok(())
}

// Makes the new directory entry of `path` durable before the source goes away. Not every
// filesystem can sync a directory, so a failure is only logged.
#[cfg(unix)]
fn sync_parent(path: &Path) {
    let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return;
    };
    if let Err(e) = File::open(parent).and_then(|d| d.sync_all()) {
        debug!(?parent, "error flushing the directory to disk: {e:#}");
    }
}

// Directories can't be opened as files here.
#[cfg(not(unix))]
fn sync_parent(_path: &Path) {}

/// Whether the two paths name the same existing file (following links): a change of case
/// on a case-insensitive filesystem, another name of the same directory, a hard link.
pub(crate) fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (fs::metadata(a), fs::metadata(b)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        // The canonical path has the case, and the volume, the filesystem stores.
        match (fs::canonicalize(a), fs::canonicalize(b)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
    }
}

/// `path` with links, `.` and the spelling of the existing part resolved by the filesystem
/// (as far as it exists; the missing rest is appended as given), to compare folders by.
pub(crate) fn resolve_path(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut missing = Vec::new();
    loop {
        let probe = if existing.as_os_str().is_empty() {
            Path::new(".")
        } else {
            existing
        };
        if let Ok(mut resolved) = fs::canonicalize(probe) {
            resolved.extend(missing.iter().rev());
            return resolved;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name.to_owned());
                existing = parent;
            }
            _ => return path.to_owned(),
        }
    }
}

// Whether the two files hold the same bytes.
fn same_content(a: &Path, b: &Path, cancel: &CancellationToken) -> anyhow::Result<bool> {
    let mut fa = File::open(a).with_context(|| format!("error opening {a:?}"))?;
    let mut fb = File::open(b).with_context(|| format!("error opening {b:?}"))?;
    let mut ba = vec![0u8; COPY_BUFFER_LEN];
    let mut bb = vec![0u8; COPY_BUFFER_LEN];
    loop {
        if cancel.is_cancelled() {
            bail!("cancelled while comparing {a:?} with {b:?}");
        }
        let read = read_full(&mut fa, &mut ba).with_context(|| format!("error reading {a:?}"))?;
        let read_b = read_full(&mut fb, &mut bb).with_context(|| format!("error reading {b:?}"))?;
        if ba.get(..read) != bb.get(..read_b) {
            return Ok(false);
        }
        if read == 0 {
            return Ok(true);
        }
    }
}

// Reads until `buf` is full or the file ends.
fn read_full(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while let Some(rest) = buf.get_mut(filled..).filter(|r| !r.is_empty()) {
        match f.read(rest) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
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

// The length of the file at `path`, following a link; `None` if there's no file. A link to
// nothing counts as no file: it has no data to move.
fn file_len(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .ok()
        .filter(|m| m.is_file())
        .map(|m| m.len())
}

// How many names `aside_path` tries.
const MAX_ASIDE_NAMES: u32 = 1000;

// The `n`th name next to `path` to keep what was there aside under.
fn aside_candidate(path: &Path, n: u32) -> anyhow::Result<PathBuf> {
    let name = path
        .file_name()
        .with_context(|| format!("{path:?} has no file name"))?
        .to_string_lossy();
    Ok(path.with_file_name(format!(".{name}.rqbit-replaced-{n}")))
}

// A free name next to `path` to keep what was there aside while it's replaced: the first
// free one, so the names in use are consecutive and the last one is the newest.
fn aside_path(path: &Path) -> anyhow::Result<PathBuf> {
    for n in 0..MAX_ASIDE_NAMES {
        let candidate = aside_candidate(path, n)?;
        if !exists(&candidate) {
            return Ok(candidate);
        }
    }
    bail!("no free name to set {path:?} aside")
}

// The name `aside_path` gave last for `path`, if it's in use.
fn last_aside(path: &Path) -> Option<PathBuf> {
    let mut last = None;
    for n in 0..MAX_ASIDE_NAMES {
        let candidate = aside_candidate(path, n).ok()?;
        if !exists(&candidate) {
            break;
        }
        last = Some(candidate);
    }
    last
}

/// Puts files back at `src` after a move from `src` to `dst` (one pair per file) was
/// interrupted by a crash; a move that fails undoes itself. The pairs are the files that
/// existed at `src` when the move started. For each: a half-done copy at either end is
/// removed; if the file is no longer at `src` but at `dst`, it's moved back; then a file
/// that an [`MoveStorageMode::Overwrite`] set aside at `dst` gets its name back. Nothing else
/// is touched. Returns a description of every file it couldn't put back.
pub(crate) fn undo_interrupted_move(pairs: &[(PathBuf, PathBuf)]) -> Result<(), String> {
    let never = CancellationToken::new();
    let mut failures = Vec::new();
    for (src, dst) in pairs {
        if src == dst {
            continue;
        }
        for path in [src, dst] {
            if let Ok(partial) = partial_path(path) {
                remove_leftover(&partial);
            }
        }
        if !exists(src) && exists(dst) {
            let undone = create_parents(src).and_then(|_| move_file(dst, src, &never));
            if let Err(e) = undone {
                failures.push(format!("{src:?} is at {dst:?} ({e:#})"));
                continue;
            }
        }
        if !exists(dst)
            && let Some(aside) = last_aside(dst)
            && let Err(e) = fs::rename(&aside, dst)
        {
            failures.push(format!(
                "the file that was at {dst:?} is at {aside:?} ({e:#})"
            ));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
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
    /// What the move did, to tell the caller.
    pub(crate) fn outcome(&self) -> MoveStorageOutcome {
        MoveStorageOutcome {
            adopted_files: self.adopted_files.clone(),
        }
    }

    /// Finishes the move: removes what was replaced, the sources of adopted files that
    /// hold the same bytes, and the source directories that became empty.
    pub(crate) fn commit(self) {
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
/// anything is touched; on error everything moved so far is moved back. Folders are compared
/// as the filesystem resolves them (links, `.`, `..`, case): another name of the old folder
/// moves nothing.
pub(crate) fn move_files(
    old_folder: &Path,
    request: &MoveStorageRequest<'_>,
) -> anyhow::Result<MovedFiles> {
    let new_folder = request.new_output_folder;
    let mut moved = MovedFiles {
        old_folder: old_folder.to_owned(),
        steps: Vec::new(),
        created_dirs: Vec::new(),
        adopted_files: Vec::new(),
    };
    let old_resolved = resolve_path(old_folder);
    let new_resolved = resolve_path(new_folder);
    if old_resolved == new_resolved {
        return Ok(moved);
    }
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
        .map(|(_, rel)| path_key(&old_resolved.join(rel)))
        .collect::<std::collections::HashSet<_>>();
    for (_, rel) in &files {
        if !is_plain_relative(rel) {
            bail!("{rel:?} isn't a relative path inside the output folder");
        }
        let dst = new_folder.join(rel);
        if sources.contains(&path_key(&new_resolved.join(rel))) {
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
        // Another name of the source (a hard link, a linked directory): the file is at the
        // destination already, and removing the source could remove it.
        if same_file(&src, &dst) {
            return Ok(());
        }
        let remove_src = match request.mode {
            MoveStorageMode::FailIfExists => bail!("{dst:?} already exists"),
            MoveStorageMode::KeepExisting => match src_len {
                // Only the same bytes make the source redundant.
                Some(len) if Some(len) == dst_len && same_content(&src, &dst, request.cancel)? => {
                    Some(Some(src.clone()))
                }
                _ => Some(None),
            },
            // With nothing to replace it with, what's there stays and is used.
            MoveStorageMode::Overwrite if src_len.is_none() => Some(None),
            MoveStorageMode::Overwrite => None,
        };
        if let Some(remove_src) = remove_src {
            // A file identical to the torrent's own needs no check.
            if remove_src.is_none() {
                moved.adopted_files.push(idx);
            }
            moved.steps.push(Step::Adopted {
                relative: rel.to_owned(),
                remove_src,
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
        assert!(moved.outcome().adopted_files.is_empty());
        moved.commit();
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
            write(&new.join("same"), b"ours");
            // Same length, other bytes.
            write(&new.join("diff"), b"THEM");
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
        let moved = move_files(
            &old,
            &request(&new, &files, MoveStorageMode::Overwrite, &cancel),
        )
        .unwrap();
        assert!(moved.outcome().adopted_files.is_empty());
        moved.commit();
        assert_eq!(read(&new.join("same")), b"ours");
        assert_eq!(read(&new.join("diff")), b"ours");
        assert!(!old.join("same").exists());
        assert_eq!(std::fs::read_dir(&new).unwrap().count(), 2, "no leftovers");

        let (_dir, old, new) = setup();
        let moved = move_files(
            &old,
            &request(&new, &files, MoveStorageMode::KeepExisting, &cancel),
        )
        .unwrap();
        assert_eq!(
            moved.outcome().adopted_files,
            vec![1],
            "an identical file isn't adopted, it's ours"
        );
        moved.commit();
        assert_eq!(read(&new.join("same")), b"ours");
        assert_eq!(read(&new.join("diff")), b"THEM");
        assert!(!old.join("same").exists(), "same bytes: source removed");
        assert_eq!(read(&old.join("diff")), b"ours", "other bytes: source kept");
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
