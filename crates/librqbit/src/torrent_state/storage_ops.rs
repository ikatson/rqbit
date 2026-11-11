// The parts of Session::force_recheck, Session::move_storage and Session::rename_file that
// act on the torrent itself.
//
// A storage operation holds `ManagedTorrent::storage_op` for its whole length, so two of
// them never interleave on one torrent: the second one waits for the first. Session::pause,
// unpause and delete take it too, so the state doesn't change under an operation (delete
// first cancels `storage_op_cancel`, which makes a running operation give up).
//
// An operation first "quiesces" the torrent: a live torrent is paused (keeping its
// counters), a running check is stopped the way a pause stops it. It then borrows the
// storage out of the state, works on it in a blocking thread without holding the state
// lock, puts it back, and restarts the torrent if it stopped it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use tracing::warn;

use super::{
    ManagedTorrent, ManagedTorrentState, TorrentMetadata, initializing::TorrentStateInitializing,
};
use crate::storage::TorrentStorage;
use crate::type_aliases::FileStorage;

/// What quiescing a torrent stopped, so that it can be restarted afterwards.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Quiesced {
    // The torrent was live and was paused for the operation.
    pub was_live: bool,
    // A check (initial or recheck) was running and was stopped for the operation.
    pub interrupted_check: bool,
}

impl Quiesced {
    /// Whether the torrent has to be started again once the operation is over.
    pub fn needs_restart(&self) -> bool {
        self.was_live || self.interrupted_check
    }
}

// What the state held when the storage was borrowed out of it.
enum Borrowed {
    Paused,
    Initializing(Arc<TorrentStateInitializing>),
    // A torrent in error has no storage: one was made for the operation and is dropped
    // after it.
    Error,
}

impl ManagedTorrent {
    /// Waits for the other storage operations on this torrent to finish. Fails if the
    /// torrent was deleted or the session stopped meanwhile.
    pub(crate) async fn lock_storage_op(&self) -> anyhow::Result<tokio::sync::MutexGuard<'_, ()>> {
        let guard = tokio::select! {
            g = self.storage_op.lock() => g,
            _ = self.storage_op_cancel.cancelled() => {
                bail!("the torrent was deleted or the session stopped")
            }
        };
        if self.storage_op_cancel.is_cancelled() {
            bail!("the torrent was deleted or the session stopped");
        }
        Ok(guard)
    }

    /// Whether the torrent was deleted or the session stopped.
    pub(crate) fn is_storage_op_cancelled(&self) -> bool {
        self.storage_op_cancel.is_cancelled()
    }

    /// Stops the torrent's I/O: pauses it if it's live, stops its check if one runs. Returns
    /// once the state is paused, initializing without a check running, or error. The caller
    /// holds `storage_op`.
    pub(crate) async fn quiesce(&self) -> anyhow::Result<Quiesced> {
        let mut q = Quiesced::default();
        loop {
            let notified = self.state_change_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut g = self.locked.write();
                match &g.state {
                    ManagedTorrentState::Live(live) => {
                        let carried = live.carried_stats();
                        let mut paused = live.pause()?;
                        paused.carried_stats = carried;
                        g.state = ManagedTorrentState::Paused(paused);
                        self.state_change_notify.notify_waiters();
                        q.was_live = true;
                        return Ok(q);
                    }
                    // The check notices the request between two pieces, then wakes us up
                    // (see `ManagedTorrent::start`).
                    ManagedTorrentState::Initializing(init) if init.is_check_running() => {
                        init.request_pause();
                        q.interrupted_check = true;
                    }
                    ManagedTorrentState::Initializing(_)
                    | ManagedTorrentState::Paused(_)
                    | ManagedTorrentState::Error(_) => return Ok(q),
                    ManagedTorrentState::None => bail!("bug: torrent is in empty state"),
                }
            }
            tokio::select! {
                _ = notified => {}
                _ = self.storage_op_cancel.cancelled() => {
                    bail!("the torrent was deleted or the session stopped")
                }
            }
        }
    }

    /// Runs `f` on the torrent's storage in a blocking thread, without holding the state
    /// lock. The torrent must be quiesced. In error state there's no storage: one is created
    /// for `f` (like resuming from error would) and dropped afterwards.
    pub(crate) async fn with_quiesced_storage<R: Send>(
        &self,
        f: impl FnOnce(&dyn TorrentStorage) -> anyhow::Result<R> + Send,
    ) -> anyhow::Result<R> {
        let metadata = self
            .metadata
            .load_full()
            .context("torrent is not resolved")?;
        let taken = {
            let mut g = self.locked.write();
            match &mut g.state {
                ManagedTorrentState::Paused(p) => Some((p.files.take()?, Borrowed::Paused)),
                ManagedTorrentState::Initializing(init) => {
                    Some((init.files.take()?, Borrowed::Initializing(init.clone())))
                }
                ManagedTorrentState::Error(_) => None,
                ManagedTorrentState::Live(_) => bail!("bug: the torrent wasn't quiesced"),
                ManagedTorrentState::None => bail!("bug: torrent is in empty state"),
            }
        };
        let (storage, borrowed): (FileStorage, Borrowed) = match taken {
            Some(taken) => taken,
            None => {
                let storage = self.shared.spawner.block_in_place(|| {
                    self.shared
                        .storage_factory
                        .create_and_init(&self.shared, &metadata)
                })?;
                (storage, Borrowed::Error)
            }
        };

        let result = self
            .shared
            .spawner
            .block_in_place_with_semaphore(|| f(&*storage))
            .await;

        // Pause, unpause and delete wait for the operation, and nothing else replaces a
        // paused or idle initializing state, so the state is still the one borrowed from.
        let mut g = self.locked.write();
        match (&mut g.state, borrowed) {
            (ManagedTorrentState::Paused(p), Borrowed::Paused) => p.files = storage,
            (ManagedTorrentState::Initializing(cur), Borrowed::Initializing(init))
                if Arc::ptr_eq(cur, &init) =>
            {
                *cur = Arc::new(
                    TorrentStateInitializing::new(
                        self.shared.clone(),
                        metadata,
                        init.only_files.clone(),
                        storage,
                        init.previously_errored(),
                    )
                    .with_skip_check(init.skip_check())
                    .with_carried_stats(init.carried_stats()),
                );
            }
            (_, Borrowed::Error) => {}
            (state, _) => warn!(
                id = self.shared.id,
                state = state.name(),
                "the torrent changed state during a storage operation, dropping its storage"
            ),
        }
        result
    }

    /// Replaces the state of a quiesced torrent with a check that ignores the fastresume
    /// bitfield and keeps the counters. A torrent in error is left as it is: starting it
    /// checks it from scratch already. The caller starts the torrent afterwards.
    pub(crate) fn begin_recheck(&self) -> anyhow::Result<()> {
        let metadata = self
            .metadata
            .load_full()
            .context("torrent is not resolved")?;
        let mut g = self.locked.write();
        let only_files = g.only_files.clone();
        let (files, carried) = match &mut g.state {
            ManagedTorrentState::Paused(p) => (p.files.take()?, p.carried_stats),
            ManagedTorrentState::Initializing(init) => (init.files.take()?, init.carried_stats()),
            ManagedTorrentState::Error(_) => return Ok(()),
            ManagedTorrentState::Live(_) => bail!("bug: the torrent wasn't quiesced"),
            ManagedTorrentState::None => bail!("bug: torrent is in empty state"),
        };
        // "previously errored" is what makes the check drop the stored bitfield and hash
        // everything.
        let init =
            TorrentStateInitializing::new(self.shared.clone(), metadata, only_files, files, true)
                .with_carried_stats(carried);
        g.state = ManagedTorrentState::Initializing(Arc::new(init));
        self.state_change_notify.notify_waiters();
        Ok(())
    }

    /// Whether the torrent is in error state.
    pub(crate) fn is_errored(&self) -> bool {
        matches!(self.locked.read().state, ManagedTorrentState::Error(_))
    }
}

// A path compared the conservative way: component by component, ignoring case, as on a
// case-insensitive filesystem.
fn path_key(path: &Path) -> Vec<String> {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect()
}

fn check_plain_relative(path: &Path) -> anyhow::Result<()> {
    if path.as_os_str().is_empty() {
        bail!("the path is empty");
    }
    for c in path.components() {
        match c {
            Component::Normal(_) => {}
            Component::ParentDir => bail!("{path:?} contains \"..\""),
            Component::CurDir => bail!("{path:?} contains \".\""),
            Component::RootDir | Component::Prefix(_) => {
                bail!("{path:?} isn't a relative path")
            }
        }
    }
    Ok(())
}

/// Checks file renames (file index -> path relative to the output folder) against the
/// torrent: the index exists and isn't a padding file, the path is relative without `..`,
/// and the file's new path doesn't collide, ignoring case, with the path of another file of
/// the torrent, nor with a directory that holds one (or the other way around). Returns the
/// renames without those that give a file its published path back.
pub(crate) fn validate_renames(
    metadata: &TorrentMetadata,
    renamed: &BTreeMap<usize, PathBuf>,
) -> anyhow::Result<BTreeMap<usize, PathBuf>> {
    let file_infos = &metadata.file_infos;
    for (&idx, path) in renamed {
        let fi = file_infos
            .get(idx)
            .with_context(|| format!("there's no file {idx} in the torrent"))?;
        if fi.attrs.padding {
            bail!("file {idx} is a padding file, it can't be renamed");
        }
        check_plain_relative(path).with_context(|| format!("can't rename file {idx}"))?;
    }

    let effective = |idx: usize| -> &Path {
        renamed
            .get(&idx)
            .map(|p| p.as_path())
            .unwrap_or(&file_infos[idx].relative_filename)
    };

    // Who's at each path and which directories the files need, for every file but padding.
    let mut by_key: HashMap<Vec<String>, Vec<usize>> = HashMap::new();
    let mut dirs: HashSet<Vec<String>> = HashSet::new();
    for (idx, fi) in file_infos.iter().enumerate() {
        if fi.attrs.padding {
            continue;
        }
        let key = path_key(effective(idx));
        for n in 1..key.len() {
            dirs.insert(key[..n].to_vec());
        }
        by_key.entry(key).or_default().push(idx);
    }

    for (&idx, path) in renamed {
        let key = path_key(path);
        if let Some(other) = by_key.get(&key).and_then(|v| v.iter().find(|&&o| o != idx)) {
            bail!(
                "can't rename file {idx} to {path:?}: file {other} is stored at {:?}",
                effective(*other)
            );
        }
        if dirs.contains(&key) {
            bail!("can't rename file {idx} to {path:?}: it's a directory holding other files");
        }
        for n in 1..key.len() {
            if let Some(other) = by_key.get(&key[..n]).and_then(|v| v.first()) {
                bail!(
                    "can't rename file {idx} to {path:?}: file {other} is stored at {:?}",
                    effective(*other)
                );
            }
        }
    }

    Ok(renamed
        .iter()
        .filter(|(idx, path)| file_infos[**idx].relative_filename != **path)
        .map(|(idx, path)| (*idx, path.clone()))
        .collect())
}
