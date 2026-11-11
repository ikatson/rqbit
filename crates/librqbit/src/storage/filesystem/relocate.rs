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

/// The filesystem calls that decide how a file moves: the rename, and the writes and flushes
/// of a copy. [`OsFs`] makes them; tests put their own in place to reach what one filesystem
/// can't show (a rename across devices, a disk that fills up, the flushes made).
pub(crate) trait FsOps {
    /// Renames `from` to `to`, as [`fs::rename`].
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Writes all of `buf` at the current position of `file`, which is open at `path`.
    fn write_all(&self, file: &mut File, path: &Path, buf: &[u8]) -> io::Result<()>;
    /// Flushes the data and metadata of `file`, which is open at `path`, to disk.
    fn sync_file(&self, file: &File, path: &Path) -> io::Result<()>;
    /// Flushes the entries of the directory `dir` to disk.
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;
}

/// [`FsOps`] made by the operating system.
pub(crate) struct OsFs;

impl FsOps for OsFs {
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }

    fn write_all(&self, file: &mut File, _path: &Path, buf: &[u8]) -> io::Result<()> {
        file.write_all(buf)
    }

    fn sync_file(&self, file: &File, _path: &Path) -> io::Result<()> {
        file.sync_all()
    }

    #[cfg(unix)]
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        File::open(dir).and_then(|d| d.sync_all())
    }

    // Directories can't be opened as files here.
    #[cfg(not(unix))]
    fn sync_dir(&self, _dir: &Path) -> io::Result<()> {
        Ok(())
    }
}

/// Moves the file `src` to `dst`, which must not exist: a rename when the two are on the
/// same filesystem, otherwise a copy (flushed to disk and length-checked) followed by the
/// removal of `src`. A symbolic link is moved as a link to the same file. On error, `src` is
/// where it was and `dst` doesn't exist.
pub(crate) fn move_file(
    fs_ops: &dyn FsOps,
    src: &Path,
    dst: &Path,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    if fs::symlink_metadata(src).is_ok_and(|m| m.file_type().is_symlink()) {
        return move_symlink(src, dst);
    }
    match fs_ops.rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            debug!(?src, ?dst, "can't rename across filesystems, copying");
            copy_then_remove(fs_ops, src, dst, cancel)
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
    fs_ops: &dyn FsOps,
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
    let copied = copy_and_sync(fs_ops, src, to, &partial, cancel)
        .and_then(|()| publish(fs_ops, &partial, dst));
    if let Err(e) = copied {
        remove_leftover(&partial);
        return Err(e);
    }
    sync_parent(fs_ops, dst);
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
fn publish(fs_ops: &dyn FsOps, partial: &Path, dst: &Path) -> anyhow::Result<()> {
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
            fs_ops
                .rename(partial, dst)
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
    fs_ops: &dyn FsOps,
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
            fs_ops
                .write_all(&mut to, dst, chunk)
                .with_context(|| format!("error writing {dst:?}"))?;
        }
        total += len;
    }
    // Gives the file the zeros it ends with, if it does.
    to.set_len(total)
        .with_context(|| format!("error setting the length of {dst:?}"))?;
    fs_ops
        .sync_file(&to, dst)
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
fn sync_parent(fs_ops: &dyn FsOps, path: &Path) {
    let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return;
    };
    if let Err(e) = fs_ops.sync_dir(parent) {
        debug!(?parent, "error flushing the directory to disk: {e:#}");
    }
}

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
            let undone = create_parents(src).and_then(|_| move_file(&OsFs, dst, src, &never));
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
    pub(crate) fn rollback(self, fs_ops: &dyn FsOps) -> Result<(), String> {
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
            if let Err(e) = move_file(fs_ops, &dst, &src, &never) {
                failures.push(format!("{src:?} is at {dst:?} ({e:#})"));
                if let Some(aside) = aside {
                    failures.push(format!("the file that was at {dst:?} is at {aside:?}"));
                }
                continue;
            }
            if let Some(aside) = aside
                && let Err(e) = fs_ops.rename(&aside, &dst)
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

fn with_rollback(fs_ops: &dyn FsOps, moved: MovedFiles, error: anyhow::Error) -> anyhow::Error {
    match moved.rollback(fs_ops) {
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
    fs_ops: &dyn FsOps,
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
        if let Err(e) = move_one(fs_ops, &mut moved, idx, rel, new_folder, request) {
            return Err(with_rollback(fs_ops, moved, e));
        }
    }
    Ok(moved)
}

fn move_one(
    fs_ops: &dyn FsOps,
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
        fs_ops
            .rename(&dst, &aside)
            .with_context(|| format!("error setting {dst:?} aside to {aside:?}"))?;
        if let Err(e) = move_file(fs_ops, &src, &dst, request.cancel) {
            if let Err(re) = fs_ops.rename(&aside, &dst) {
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
    move_file(fs_ops, &src, &dst, request.cancel)?;
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
    use std::{
        cell::{Cell, RefCell},
        fs::File,
        io,
        path::{Path, PathBuf},
    };

    use tokio_util::sync::CancellationToken;

    use super::{
        COPY_BUFFER_LEN, FsOps, OsFs, copy_then_remove, create_parents, move_file, move_files,
        partial_path, remove_empty_parents,
    };
    use crate::storage::{MoveStorageMode, MoveStorageRequest};

    fn write(path: &Path, content: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn read(path: &Path) -> Vec<u8> {
        std::fs::read(path).unwrap()
    }

    // Bytes no copy can mistake for a hole: no chunk of them is all zeros.
    fn pattern(len: usize) -> Vec<u8> {
        (1..=251u8).cycle().take(len).collect()
    }

    fn len_u64(len: usize) -> u64 {
        u64::try_from(len).unwrap()
    }

    // Called after every successful write with the file written and the bytes written so far
    // by the filesystem.
    type WriteHook = Box<dyn Fn(&Path, u64)>;

    /// The real filesystem, except that `device` and everything under it behave as another
    /// filesystem (a rename between the two fails as across devices), the disk can fill up
    /// once, and every flush and write is recorded.
    #[derive(Default)]
    struct TestFs {
        device: Option<PathBuf>,
        // How many bytes the disk takes before the next write fails as full; only once, so
        // that an undo afterwards can copy files back.
        full_after: Cell<Option<u64>>,
        written: Cell<u64>,
        // The rename call (counted from 0) after which `.1` gets cancelled.
        cancel_after_rename: Option<(usize, CancellationToken)>,
        renames: Cell<usize>,
        after_write: Option<WriteHook>,
        synced_files: RefCell<Vec<PathBuf>>,
        synced_dirs: RefCell<Vec<PathBuf>>,
    }

    impl TestFs {
        fn across(device: &Path) -> Self {
            Self {
                device: Some(device.to_owned()),
                ..Default::default()
            }
        }
    }

    impl FsOps for TestFs {
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            let call = self.renames.get();
            self.renames.set(call + 1);
            let crosses = self
                .device
                .as_deref()
                .is_some_and(|d| from.starts_with(d) != to.starts_with(d));
            let result = if crosses {
                Err(io::Error::from(io::ErrorKind::CrossesDevices))
            } else {
                OsFs.rename(from, to)
            };
            if let Some((at, cancel)) = &self.cancel_after_rename
                && *at == call
            {
                cancel.cancel();
            }
            result
        }

        fn write_all(&self, file: &mut File, path: &Path, buf: &[u8]) -> io::Result<()> {
            let written = self.written.get();
            if let Some(room) = self.full_after.get()
                && written + len_u64(buf.len()) > room
            {
                self.full_after.set(None);
                let fits = usize::try_from(room - written).unwrap();
                OsFs.write_all(file, path, &buf[..fits])?;
                self.written.set(room);
                return Err(io::Error::from(io::ErrorKind::StorageFull));
            }
            OsFs.write_all(file, path, buf)?;
            self.written.set(written + len_u64(buf.len()));
            if let Some(hook) = &self.after_write {
                hook(path, self.written.get());
            }
            Ok(())
        }

        fn sync_file(&self, file: &File, path: &Path) -> io::Result<()> {
            self.synced_files.borrow_mut().push(path.to_owned());
            OsFs.sync_file(file, path)
        }

        fn sync_dir(&self, dir: &Path) -> io::Result<()> {
            self.synced_dirs.borrow_mut().push(dir.to_owned());
            OsFs.sync_dir(dir)
        }
    }

    // The names in `dir`, sorted; none if it doesn't exist.
    fn names(dir: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut names = entries
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    fn io_kind_in_chain(err: &anyhow::Error, kind: io::ErrorKind) -> bool {
        err.chain().any(|e| {
            e.downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == kind)
        })
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
        copy_then_remove(&OsFs, &src, &dst, &CancellationToken::new()).unwrap();
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
        copy_then_remove(&OsFs, &src, &dst, &cancel).unwrap_err();
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
            &OsFs,
            &old,
            &request(&new, &files, MoveStorageMode::FailIfExists, &cancel),
        )
        .unwrap();
        assert_eq!(read(&new.join("d/one")), b"one");
        moved.rollback(&OsFs).unwrap();
        assert_eq!(read(&old.join("d/one")), b"one");
        assert_eq!(read(&old.join("two")), b"two");
        assert!(!new.join("d").exists(), "created directories are removed");

        let moved = move_files(
            &OsFs,
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
            &OsFs,
            &old,
            &request(&new, &files, MoveStorageMode::FailIfExists, &cancel),
        )
        .err()
        .unwrap();
        assert!(format!("{err:#}").contains("already exists"), "{err:#}");
        assert_eq!(read(&old.join("same")), b"ours");

        let (_dir, old, new) = setup();
        let moved = move_files(
            &OsFs,
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
            &OsFs,
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
            &OsFs,
            &old,
            &request(&new, &files, MoveStorageMode::Overwrite, &cancel),
        )
        .unwrap();
        assert_eq!(read(&new.join("a")), b"ours");
        moved.rollback(&OsFs).unwrap();
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
            &OsFs,
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
            &OsFs,
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
            &OsFs,
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
            &OsFs,
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

    #[test]
    fn test_move_files_across_devices_copies_then_commits_or_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        // More than one copy buffer, so the copy loop runs several times.
        let big = pattern(COPY_BUFFER_LEN * 2 + 17);
        write(&old.join("a/big"), &big);
        write(&old.join("small"), b"small");
        let files = vec![Some(PathBuf::from("a/big")), Some(PathBuf::from("small"))];
        let total = len_u64(big.len() + b"small".len());
        let cancel = CancellationToken::new();

        let mut fs = TestFs::across(&new);
        let (o, n) = (old.clone(), new.clone());
        // While a file is being copied (either way), its source is still there and the
        // copy doesn't have the file's name yet.
        fs.after_write = Some(Box::new(move |partial, _| {
            let name = partial.file_name().unwrap().to_str().unwrap();
            let name = name
                .strip_prefix('.')
                .and_then(|n| n.strip_suffix(".rqbit-partial"))
                .expect("copies are written under a temporary name");
            let (from, to) = if partial.starts_with(&n) {
                (&o, &n)
            } else {
                (&n, &o)
            };
            let rel = partial
                .parent()
                .unwrap()
                .strip_prefix(to)
                .unwrap()
                .join(name);
            assert!(from.join(&rel).exists(), "the source stays during its copy");
            assert!(!to.join(&rel).exists(), "the copy is named once complete");
        }));

        let moved = move_files(
            &fs,
            &old,
            &request(&new, &files, MoveStorageMode::FailIfExists, &cancel),
        )
        .unwrap();
        assert_eq!(fs.written.get(), total, "copied, not renamed");
        assert_eq!(read(&new.join("a/big")), big);
        assert_eq!(
            std::fs::metadata(new.join("a/big")).unwrap().len(),
            len_u64(big.len())
        );
        assert_eq!(read(&new.join("small")), b"small");
        assert!(!old.join("a/big").exists() && !old.join("small").exists());
        assert_eq!(names(&new), ["a", "small"], "no temporary copies left");
        assert_eq!(names(&new.join("a")), ["big"], "no temporary copies left");

        moved.rollback(&fs).unwrap();
        assert_eq!(
            fs.written.get(),
            2 * total,
            "the undo copies the files back"
        );
        assert_eq!(read(&old.join("a/big")), big);
        assert_eq!(read(&old.join("small")), b"small");
        assert_eq!(names(&old), ["a", "small"]);
        assert_eq!(names(&old.join("a")), ["big"]);
        assert!(
            !new.exists(),
            "the copies and the directories made are gone"
        );

        let moved = move_files(
            &fs,
            &old,
            &request(&new, &files, MoveStorageMode::FailIfExists, &cancel),
        )
        .unwrap();
        moved.commit();
        assert_eq!(fs.written.get(), 3 * total);
        assert_eq!(read(&new.join("a/big")), big);
        assert_eq!(read(&new.join("small")), b"small");
        assert!(names(&old).is_empty(), "emptied source directories go");
    }

    #[test]
    fn test_cross_device_copy_checks_the_length() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a");
        let other = dir.path().join("other");
        let dst = other.join("a");
        std::fs::create_dir(&other).unwrap();
        let content = pattern(COPY_BUFFER_LEN * 2 + 17);
        write(&src, &content);
        let shrunk = COPY_BUFFER_LEN + 5;

        let mut fs = TestFs::across(&other);
        let s = src.clone();
        // Something else shortens the source while it's being copied.
        fs.after_write = Some(Box::new(move |_, written| {
            if written == len_u64(COPY_BUFFER_LEN) {
                File::options()
                    .write(true)
                    .open(&s)
                    .unwrap()
                    .set_len(len_u64(shrunk))
                    .unwrap();
            }
        }));
        let err = move_file(&fs, &src, &dst, &CancellationToken::new()).unwrap_err();
        assert!(
            format!("{err:#}").contains("bytes long instead of"),
            "{err:#}"
        );
        assert!(names(&other).is_empty(), "no copy, partial or not, is left");
        assert_eq!(read(&src), content[..shrunk], "the source stays");
    }

    // The bytes the filesystem gave `path`, if it can tell.
    #[cfg(unix)]
    fn allocated(path: &Path) -> Option<u64> {
        use std::os::unix::fs::MetadataExt;
        // st_blocks counts 512-byte units whatever the block size of the filesystem.
        Some(std::fs::metadata(path).ok()?.blocks() * 512)
    }

    #[cfg(not(unix))]
    fn allocated(_path: &Path) -> Option<u64> {
        None
    }

    #[test]
    fn test_cross_device_copy_keeps_holes() {
        use std::io::{Seek, SeekFrom, Write};

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a");
        let other = dir.path().join("other");
        let dst = other.join("a");
        std::fs::create_dir(&other).unwrap();
        // A hole, data, and a hole up to the end, as a preallocated torrent file has.
        let len = COPY_BUFFER_LEN * 6;
        let data_at = COPY_BUFFER_LEN * 2;
        let data = pattern(COPY_BUFFER_LEN);
        {
            let mut f = File::create(&src).unwrap();
            f.set_len(len_u64(len)).unwrap();
            f.seek(SeekFrom::Start(len_u64(data_at))).unwrap();
            f.write_all(&data).unwrap();
        }
        let mut expected = vec![0u8; len];
        expected[data_at..data_at + data.len()].copy_from_slice(&data);
        let src_allocated = allocated(&src);

        let fs = TestFs::across(&other);
        move_file(&fs, &src, &dst, &CancellationToken::new()).unwrap();
        assert_eq!(
            fs.written.get(),
            len_u64(data.len()),
            "only the data is written, the holes are skipped"
        );
        assert_eq!(read(&dst), expected);
        assert!(!src.exists());
        match (src_allocated, allocated(&dst)) {
            (Some(src_allocated), Some(dst_allocated)) if src_allocated < len_u64(len) => {
                assert!(
                    dst_allocated <= src_allocated + len_u64(COPY_BUFFER_LEN),
                    "the copy takes {dst_allocated} bytes on disk, the source {src_allocated}"
                );
            }
            _ => eprintln!(
                "the temporary filesystem doesn't keep holes or can't tell: only the length \
                 and the bytes of the copy were checked"
            ),
        }
    }

    #[test]
    fn test_cross_device_copy_flushes_the_file_and_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a");
        let other = dir.path().join("other/deep");
        let dst = other.join("a");
        std::fs::create_dir_all(&other).unwrap();
        write(&src, b"data");
        let fs = TestFs::across(&dir.path().join("other"));
        move_file(&fs, &src, &dst, &CancellationToken::new()).unwrap();
        assert_eq!(read(&dst), b"data");
        assert_eq!(
            *fs.synced_files.borrow(),
            [partial_path(&dst).unwrap()],
            "the copy is flushed while it still has its temporary name"
        );
        assert_eq!(
            *fs.synced_dirs.borrow(),
            [other],
            "the directory that got the copy's name is flushed"
        );
    }

    #[test]
    fn test_full_disk_part_way_undoes_the_move() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        let one = pattern(100);
        let two = pattern(COPY_BUFFER_LEN + 3);
        let three = pattern(COPY_BUFFER_LEN * 2);
        write(&old.join("1"), &one);
        write(&old.join("d/2"), &two);
        write(&old.join("3"), &three);
        let files = vec![
            Some(PathBuf::from("1")),
            Some(PathBuf::from("d/2")),
            Some(PathBuf::from("3")),
        ];
        let fs = TestFs::across(&new);
        let before_three = len_u64(one.len() + two.len());
        // The disk fills half way through the first chunk of "3".
        let half_chunk = len_u64(COPY_BUFFER_LEN / 2);
        fs.full_after.set(Some(before_three + half_chunk));
        let cancel = CancellationToken::new();

        let err = move_files(
            &fs,
            &old,
            &request(&new, &files, MoveStorageMode::FailIfExists, &cancel),
        )
        .err()
        .unwrap();
        assert!(
            io_kind_in_chain(&err, io::ErrorKind::StorageFull),
            "{err:#}"
        );
        assert_eq!(
            fs.written.get(),
            before_three + half_chunk + before_three,
            "\"1\" and \"d/2\" were copied there and back, part of \"3\" was copied"
        );
        assert!(!partial_path(&new.join("3")).unwrap().exists());
        assert!(
            !new.exists(),
            "the partial copy and the directories made are gone"
        );
        assert_eq!(read(&old.join("1")), one);
        assert_eq!(read(&old.join("d/2")), two);
        assert_eq!(read(&old.join("3")), three);
        assert_eq!(names(&old), ["1", "3", "d"]);
        assert_eq!(names(&old.join("d")), ["2"]);
    }

    #[cfg(unix)]
    #[test]
    fn test_links_move_as_links() {
        for across in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("target");
            write(&target, b"target bytes");
            let old = dir.path().join("old");
            // One level deeper than `old`: a relative link copied as is would point
            // elsewhere.
            let new = dir.path().join("x/new");
            std::fs::create_dir(&old).unwrap();
            std::os::unix::fs::symlink("../target", old.join("rel")).unwrap();
            std::os::unix::fs::symlink(&target, old.join("abs")).unwrap();
            let files = vec![Some(PathBuf::from("rel")), Some(PathBuf::from("abs"))];
            let fs = if across {
                TestFs::across(&new)
            } else {
                TestFs::default()
            };
            let cancel = CancellationToken::new();
            let real_target = std::fs::canonicalize(&target).unwrap();
            let check_links = |folder: &Path| {
                for name in ["rel", "abs"] {
                    let link = folder.join(name);
                    let meta = std::fs::symlink_metadata(&link).unwrap();
                    assert!(meta.file_type().is_symlink(), "{link:?} (across: {across})");
                    assert_eq!(std::fs::canonicalize(&link).unwrap(), real_target);
                }
                let meta = std::fs::symlink_metadata(&target).unwrap();
                assert!(meta.is_file(), "the target stays a file");
                assert_eq!(read(&target), b"target bytes", "the target is untouched");
            };

            let moved = move_files(
                &fs,
                &old,
                &request(&new, &files, MoveStorageMode::FailIfExists, &cancel),
            )
            .unwrap();
            check_links(&new);
            assert!(names(&old).is_empty(), "the links left the old folder");
            moved.rollback(&fs).unwrap();
            check_links(&old);
            assert!(!new.exists());

            let moved = move_files(
                &fs,
                &old,
                &request(&new, &files, MoveStorageMode::FailIfExists, &cancel),
            )
            .unwrap();
            moved.commit();
            check_links(&new);
            assert!(names(&old).is_empty());
            assert_eq!(fs.written.get(), 0, "no data is copied for a link");
        }
    }

    #[test]
    fn test_cancel_between_files_moves_back_the_moved_ones() {
        for across in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let old = dir.path().join("old");
            let new = dir.path().join("new");
            let contents: [(&str, &[u8]); 3] = [("a", b"aaa"), ("b", b"bbbb"), ("c", b"ccccc")];
            for (name, content) in contents {
                write(&old.join(name), content);
            }
            let files = contents
                .iter()
                .map(|(name, _)| Some(PathBuf::from(name)))
                .collect::<Vec<_>>();
            let cancel = CancellationToken::new();
            let mut fs = if across {
                TestFs::across(&new)
            } else {
                TestFs::default()
            };
            // Fired once "b" is reached, after "a" moved. By rename "b" moves and "c" sees
            // the cancellation; across devices the copy of "b" does.
            fs.cancel_after_rename = Some((1, cancel.clone()));

            let err = move_files(
                &fs,
                &old,
                &request(&new, &files, MoveStorageMode::FailIfExists, &cancel),
            )
            .err()
            .unwrap();
            let msg = format!("{err:#}");
            assert!(cancel.is_cancelled());
            if across {
                assert!(msg.contains("cancelled while copying"), "{msg}");
                assert_eq!(fs.written.get(), 2 * 3, "\"a\" was copied there and back");
                assert_eq!(fs.renames.get(), 3, "a, b, then a back");
            } else {
                assert!(msg.contains("the move was cancelled"), "{msg}");
                assert_eq!(fs.renames.get(), 4, "a, b, then b and a back");
            }
            for (name, content) in contents {
                assert_eq!(read(&old.join(name)), content, "{name} (across: {across})");
            }
            assert!(!new.exists(), "nothing is left at the destination");
        }
    }
}
