use std::{
    fs::OpenOptions,
    io::IoSlice,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use parking_lot::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::{
    storage::{
        MoveStorageOutcome, MoveStorageRequest, StorageFactoryExt,
        filesystem::opened_file::OurFileExt,
    },
    torrent_state::{ManagedTorrentShared, TorrentMetadata},
};

use crate::storage::{StorageFactory, TorrentStorage};

use super::{opened_file::OpenedFile, relocate};

#[derive(Default, Clone, Copy)]
pub struct FilesystemStorageFactory {}

impl StorageFactory for FilesystemStorageFactory {
    type Storage = FilesystemStorage;

    fn create(
        &self,
        shared: &ManagedTorrentShared,
        _metadata: &TorrentMetadata,
    ) -> anyhow::Result<FilesystemStorage> {
        Ok(FilesystemStorage {
            output_folder: RwLock::new(shared.output_folder()),
            opened_files: Default::default(),
            pending_move: Default::default(),
        })
    }

    fn clone_box(&self) -> crate::storage::BoxStorageFactory {
        self.boxed()
    }
}

pub struct FilesystemStorage {
    // Changed by move_storage(), which runs while the torrent does no I/O.
    output_folder: RwLock<PathBuf>,
    pub(crate) opened_files: Vec<OpenedFile>,
    // The move that move_storage() made and finish_move_storage() hasn't ended yet.
    pending_move: Mutex<Option<PendingMove>>,
}

struct PendingMove {
    moved: relocate::MovedFiles,
    old_folder: PathBuf,
    files: Vec<Option<PathBuf>>,
    was_open: Vec<bool>,
}

impl FilesystemStorage {
    #[allow(dead_code)]
    pub(crate) fn take_fs(&self) -> anyhow::Result<Self> {
        Ok(Self {
            opened_files: self
                .opened_files
                .iter()
                .map(|f| f.take_clone())
                .collect::<anyhow::Result<Vec<_>>>()?,
            output_folder: RwLock::new(self.output_folder.read().clone()),
            pending_move: Default::default(),
        })
    }

    fn output_folder(&self) -> PathBuf {
        self.output_folder.read().clone()
    }

    // Points every file of `relative_paths` at `folder` and opens the ones in `open`. Goes on
    // past a file that fails, so that one bad file doesn't leave the others closed, and
    // returns the first error.
    fn reopen_all(
        &self,
        folder: &Path,
        relative_paths: &[Option<PathBuf>],
        open: &[bool],
    ) -> anyhow::Result<()> {
        let mut first_error = None;
        for ((file, rel), open) in self.opened_files.iter().zip(relative_paths).zip(open) {
            if let Some(rel) = rel
                && let Err(e) = file.reopen(folder.join(rel), *open)
            {
                warn!("{e:#}");
                first_error.get_or_insert(e);
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl TorrentStorage for FilesystemStorage {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        self.opened_files
            .get(file_id)
            .context("no such file")?
            .lock_read()?
            .pread_exact(offset, buf)
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        let of = self.opened_files.get(file_id).context("no such file")?;
        #[cfg(windows)]
        return of.try_mark_sparse()?.pwrite_all(offset, buf);
        #[cfg(not(windows))]
        return of.lock_read()?.pwrite_all(offset, buf);
    }

    fn pwrite_all_vectored(
        &self,
        file_id: usize,
        offset: u64,
        bufs: [IoSlice<'_>; 2],
    ) -> anyhow::Result<usize> {
        let of = self.opened_files.get(file_id).context("no such file")?;
        #[cfg(windows)]
        return of.try_mark_sparse()?.pwrite_all_vectored(offset, bufs);
        #[cfg(not(windows))]
        return of.lock_read()?.pwrite_all_vectored(offset, bufs);
    }

    fn remove_file(&self, _file_id: usize, filename: &Path) -> anyhow::Result<()> {
        Ok(std::fs::remove_file(self.output_folder().join(filename))?)
    }

    fn ensure_file_length(&self, file_id: usize, len: u64) -> anyhow::Result<()> {
        let f = &self.opened_files.get(file_id).context("no such file")?;
        #[cfg(windows)]
        f.try_mark_sparse()?;
        Ok(f.lock_read()?.set_len(len)?)
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        Ok(Box::new(self.take_fs()?))
    }

    fn remove_directory_if_empty(&self, path: &Path) -> anyhow::Result<()> {
        let path = self.output_folder().join(path);
        if !path.is_dir() {
            anyhow::bail!("cannot remove dir: {path:?} is not a directory")
        }
        if std::fs::read_dir(&path)?.count() == 0 {
            std::fs::remove_dir(&path).with_context(|| format!("error removing {path:?}"))
        } else {
            warn!("did not remove {path:?} as it was not empty");
            Ok(())
        }
    }

    fn init(
        &mut self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        let output_folder = self.output_folder();
        let mut files = Vec::<OpenedFile>::new();
        for (file_id, file_details) in metadata.file_infos.iter().enumerate() {
            if file_details.attrs.padding {
                files.push(OpenedFile::new_dummy());
                continue;
            };
            // Renamed files live at their new path, not the one the torrent gives them.
            let relative_path = shared
                .file_relative_path(metadata, file_id)
                .context("bug: no such file")?;
            let full_path = output_folder.join(relative_path);
            std::fs::create_dir_all(full_path.parent().context("bug: no parent")?)?;
            let f = if shared.options.allow_overwrite {
                OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(&full_path)
                    .with_context(|| format!("error opening {full_path:?} in read/write mode"))?
            } else {
                // create_new does not seem to work with read(true), so calling this twice.
                OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&full_path)
                    .with_context(|| {
                        format!(
                            "error creating a new file (because allow_overwrite = false) {:?}",
                            full_path
                        )
                    })?;
                OpenOptions::new().read(true).write(true).open(&full_path)?
            };
            files.push(OpenedFile::new(full_path.clone(), f));
        }

        self.opened_files = files;
        Ok(())
    }

    fn file_length(&self, file_id: usize) -> anyhow::Result<Option<u64>> {
        self.opened_files
            .get(file_id)
            .context("no such file")?
            .len()
    }

    fn rename_file(&self, file_id: usize, from: &Path, to: &Path) -> anyhow::Result<()> {
        if !relocate::is_plain_relative(to) {
            bail!("{to:?} isn't a relative path inside the output folder");
        }
        let file = self.opened_files.get(file_id).context("no such file")?;
        let folder = self.output_folder();
        let src = folder.join(from);
        let dst = folder.join(to);
        file.with_closed(|_| {
            // A change of case only is the same file on a case-insensitive filesystem; on a
            // case-sensitive one it's another file, which must not be overwritten.
            if relocate::exists(&dst) && !relocate::same_file(&src, &dst) {
                bail!("{dst:?} already exists");
            }
            let created = relocate::create_parents(&dst)?;
            if relocate::exists(&src) {
                if let Err(e) = relocate::move_file(&src, &dst, &CancellationToken::new()) {
                    relocate::remove_created_dirs(&created);
                    return Err(e);
                }
                relocate::remove_empty_parents(&folder, from);
            }
            Ok(dst.clone())
        })
    }

    fn move_storage(&self, request: &MoveStorageRequest<'_>) -> anyhow::Result<MoveStorageOutcome> {
        let mut pending = self.pending_move.lock();
        if pending.is_some() {
            bail!("bug: the previous move wasn't finished");
        }
        let old_folder = self.output_folder();
        let new_folder = request.new_output_folder;
        // Windows can't rename open files; reopened below, at whichever place they end up.
        let was_open = self
            .opened_files
            .iter()
            .map(|f| f.close())
            .collect::<Vec<_>>();

        let moved = match relocate::move_files(&old_folder, request) {
            Ok(moved) => moved,
            Err(e) => {
                return Err(
                    match self.reopen_all(&old_folder, request.files, &was_open) {
                        Ok(()) => e,
                        Err(re) => e.context(format!("and reopening the files failed: {re:#}")),
                    },
                );
            }
        };
        let move_ = PendingMove {
            moved,
            old_folder,
            files: request.files.to_vec(),
            was_open,
        };
        if let Err(e) = self.reopen_all(new_folder, request.files, &move_.was_open) {
            let e = e.context(format!("error opening the files at {new_folder:?}"));
            return Err(match self.undo_move(move_) {
                Ok(()) => e,
                Err(ue) => e.context(format!("{ue:#}")),
            });
        }
        *self.output_folder.write() = new_folder.to_owned();
        let outcome = move_.moved.outcome();
        *pending = Some(move_);
        Ok(outcome)
    }

    fn finish_move_storage(&self, commit: bool) -> anyhow::Result<()> {
        let Some(move_) = self.pending_move.lock().take() else {
            // move_storage() had nothing to move.
            return Ok(());
        };
        if commit {
            move_.moved.commit();
            return Ok(());
        }
        self.undo_move(move_)
            .context("error undoing the move of the files")
    }
}

impl FilesystemStorage {
    // Moves the files of `move_` back and opens them at the old folder again. The error says
    // which files couldn't be put back.
    fn undo_move(&self, move_: PendingMove) -> anyhow::Result<()> {
        for f in &self.opened_files {
            f.close();
        }
        let mut errors = Vec::new();
        if let Err(left) = move_.moved.rollback() {
            errors.push(format!("undoing the move failed: {left}"));
        }
        if let Err(e) = self.reopen_all(&move_.old_folder, &move_.files, &move_.was_open) {
            errors.push(format!(
                "reopening the files at {:?} failed: {e:#}",
                move_.old_folder
            ));
        }
        *self.output_folder.write() = move_.old_folder;
        match errors.is_empty() {
            true => Ok(()),
            false => bail!("{}", errors.join("; ")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use parking_lot::RwLock;
    use tokio_util::sync::CancellationToken;

    use super::FilesystemStorage;
    use crate::storage::{
        MoveStorageMode, MoveStorageRequest, TorrentStorage, filesystem::opened_file::OpenedFile,
    };

    // A storage for the files `names` under `folder` (None = a padding file), each created
    // with its name as content.
    fn storage(folder: &Path, names: &[Option<&str>]) -> FilesystemStorage {
        let opened_files = names
            .iter()
            .map(|name| match name {
                None => OpenedFile::new_dummy(),
                Some(name) => {
                    let path = folder.join(name);
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(&path, name.as_bytes()).unwrap();
                    let f = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&path)
                        .unwrap();
                    OpenedFile::new(path, f)
                }
            })
            .collect();
        FilesystemStorage {
            output_folder: RwLock::new(folder.to_owned()),
            opened_files,
            pending_move: Default::default(),
        }
    }

    fn read_back(s: &FilesystemStorage, file_id: usize, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        s.pread_exact(file_id, 0, &mut buf).unwrap();
        buf
    }

    #[test]
    fn test_file_length() {
        let dir = tempfile::tempdir().unwrap();
        let s = storage(dir.path(), &[Some("a"), None]);
        assert_eq!(s.file_length(0).unwrap(), Some(1));
        assert_eq!(s.file_length(1).unwrap(), None);
        s.file_length(2).unwrap_err();
    }

    #[test]
    fn test_rename_file_keeps_the_file_usable() {
        let dir = tempfile::tempdir().unwrap();
        let s = storage(dir.path(), &[Some("d/one"), Some("two")]);
        s.rename_file(0, Path::new("d/one"), Path::new("new/sub/uno"))
            .unwrap();
        assert!(!dir.path().join("d").exists(), "emptied directory removed");
        assert_eq!(
            std::fs::read(dir.path().join("new/sub/uno")).unwrap(),
            b"d/one"
        );
        s.pwrite_all(0, 0, b"X").unwrap();
        assert_eq!(read_back(&s, 0, 5), b"X/one");
        assert_eq!(
            std::fs::read(dir.path().join("new/sub/uno")).unwrap(),
            b"X/one"
        );
    }

    #[test]
    fn test_rename_file_rejects_existing_destination_and_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let s = storage(dir.path(), &[Some("a"), Some("b")]);
        s.rename_file(0, Path::new("a"), Path::new("b"))
            .unwrap_err();
        s.rename_file(0, Path::new("a"), Path::new("../a"))
            .unwrap_err();
        s.rename_file(0, Path::new("a"), Path::new("/abs"))
            .unwrap_err();
        assert_eq!(read_back(&s, 0, 1), b"a");
        assert_eq!(std::fs::read(dir.path().join("a")).unwrap(), b"a");
    }

    #[test]
    fn test_move_storage_moves_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        let s = storage(&old, &[Some("x/a"), None, Some("b")]);
        let files = vec![Some(PathBuf::from("x/a")), None, Some(PathBuf::from("b"))];
        let cancel = CancellationToken::new();
        let outcome = s
            .move_storage(&MoveStorageRequest {
                new_output_folder: &new,
                files: &files,
                mode: MoveStorageMode::FailIfExists,
                cancel: &cancel,
            })
            .unwrap();
        assert!(outcome.adopted_files.is_empty());
        assert_eq!(s.output_folder(), new);
        s.finish_move_storage(true).unwrap();
        assert!(!old.join("x").exists());
        s.pwrite_all(2, 0, b"B").unwrap();
        assert_eq!(std::fs::read(new.join("b")).unwrap(), b"B");
        assert_eq!(read_back(&s, 0, 3), b"x/a");
        s.remove_file(2, Path::new("b")).unwrap();
        assert!(!new.join("b").exists(), "removal follows the new folder");
    }

    #[test]
    fn test_move_storage_failure_keeps_the_old_folder_working() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        let s = storage(&old, &[Some("a"), Some("b/c")]);
        // A file where "b" must become a directory makes the second file fail.
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(new.join("b"), b"in the way").unwrap();
        let files = vec![Some(PathBuf::from("a")), Some(PathBuf::from("b/c"))];
        let cancel = CancellationToken::new();
        s.move_storage(&MoveStorageRequest {
            new_output_folder: &new,
            files: &files,
            mode: MoveStorageMode::FailIfExists,
            cancel: &cancel,
        })
        .unwrap_err();
        assert_eq!(s.output_folder(), old);
        s.pwrite_all(0, 0, b"A").unwrap();
        assert_eq!(std::fs::read(old.join("a")).unwrap(), b"A");
        assert!(!new.join("a").exists());
    }
}
