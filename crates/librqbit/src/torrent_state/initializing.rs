use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

use anyhow::Context;

use rand::{RngExt, seq::IteratorRandom};
use size_format::SizeFormatterBinary as SF;
use tracing::{info, trace, warn};

use crate::{
    api::TorrentIdOrHash,
    bitv::BitV,
    bitv_factory::BitVFactory,
    chunk_tracker::{ChunkTracker, compute_selected_pieces},
    file_ops::FileOps,
    type_aliases::{BF, FileStorage},
};

const MAX_FASTRESUME_CHECKS: usize = 64;

use super::{
    ManagedTorrentShared, TorrentMetadata,
    paused::{CarriedStats, TorrentStatePaused},
};

pub struct TorrentStateInitializing {
    pub(crate) files: FileStorage,
    pub(crate) shared: Arc<ManagedTorrentShared>,
    pub(crate) metadata: Arc<TorrentMetadata>,
    pub(crate) only_files: Option<Vec<usize>>,
    pub(crate) checked_bytes: AtomicU64,
    pause_requested: AtomicBool,
    check_running: AtomicBool,
    previously_errored: bool,
    // AddTorrentOptions::skip_initial_check.
    skip_check: bool,
    carried_stats: CarriedStats,
}

impl TorrentStateInitializing {
    pub fn new(
        shared: Arc<ManagedTorrentShared>,
        metadata: Arc<TorrentMetadata>,
        only_files: Option<Vec<usize>>,
        files: FileStorage,
        previously_errored: bool,
    ) -> Self {
        Self {
            shared,
            metadata,
            only_files,
            files,
            checked_bytes: AtomicU64::new(0),
            pause_requested: AtomicBool::new(false),
            check_running: AtomicBool::new(false),
            previously_errored,
            skip_check: false,
            carried_stats: CarriedStats::default(),
        }
    }

    /// Without a valid fastresume bitfield, trust files that all have their expected length
    /// instead of hashing them ([`crate::AddTorrentOptions::skip_initial_check`]).
    pub(crate) fn with_skip_check(mut self, skip_check: bool) -> Self {
        self.skip_check = skip_check;
        self
    }

    /// The counters the live state starts from after the check.
    pub(crate) fn with_carried_stats(mut self, carried_stats: CarriedStats) -> Self {
        self.carried_stats = carried_stats;
        self
    }

    pub(crate) fn carried_stats(&self) -> CarriedStats {
        self.carried_stats
    }

    pub(crate) fn previously_errored(&self) -> bool {
        self.previously_errored
    }

    pub(crate) fn skip_check(&self) -> bool {
        self.skip_check
    }

    pub(crate) fn is_check_running(&self) -> bool {
        self.check_running.load(Ordering::Acquire)
    }

    pub fn get_checked_bytes(&self) -> u64 {
        self.checked_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn request_pause(&self) {
        self.pause_requested.store(true, Ordering::Relaxed);
    }

    pub(crate) fn clear_pause_request(&self) {
        self.pause_requested.store(false, Ordering::Relaxed);
    }

    pub(crate) fn is_pause_requested(&self) -> bool {
        self.pause_requested.load(Ordering::Relaxed)
    }

    pub(crate) fn try_start_check(&self) -> bool {
        self.check_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn finish_check(&self) {
        self.check_running.store(false, Ordering::Release);
    }

    async fn validate_fastresume(
        &self,
        bitv_factory: &dyn BitVFactory,
        have_pieces: Option<Box<dyn BitV>>,
    ) -> Option<Box<dyn BitV>> {
        let hp = have_pieces?;
        let actual = hp.as_bytes().len();
        let expected = self.metadata.lengths().piece_bitfield_bytes();
        if actual != expected {
            warn!(
                actual,
                expected,
                "the bitfield loaded isn't of correct length, ignoring it, will do full check"
            );
            return None;
        }

        let is_broken = self
            .shared
            .spawner
            .block_in_place_with_semaphore(|| {
                let fo = crate::file_ops::FileOps::new(
                    &self.metadata.info,
                    &self.files,
                    &self.metadata.file_infos,
                );

                let mut to_validate = BF::from_boxed_slice(
                    vec![0u8; self.metadata.lengths().piece_bitfield_bytes()].into_boxed_slice(),
                );
                let mut queue = hp.as_slice().to_owned();

                // Validate at least one piece from each file, if we claim we have it.
                for fi in self.metadata.file_infos.iter() {
                    let prange = fi.piece_range_usize();
                    let offset = prange.start;
                    for piece_id in hp
                        .as_slice()
                        .get(fi.piece_range_usize())
                        .into_iter()
                        .flat_map(|s| s.iter_ones())
                        .map(|pid| pid + offset)
                        .take(1)
                    {
                        to_validate.set(piece_id, true);
                        queue.set(piece_id, false);
                    }
                }

                // For all the remaining pieces we claim we have, validate them with decreasing probability.
                let queue = queue
                    .iter_ones()
                    .sample(&mut rand::rng(), MAX_FASTRESUME_CHECKS);

                for (tmp_id, piece_id) in queue.into_iter().enumerate() {
                    let denom: u32 = (tmp_id + 1).min(50).try_into().unwrap();
                    if rand::rng().random_ratio(1, denom) {
                        to_validate.set(piece_id, true);
                    }
                }

                let to_validate_count = to_validate.count_ones();
                for (id, piece_id) in to_validate
                    .iter_ones()
                    .filter_map(|id| {
                        self.metadata
                            .lengths()
                            .validate_piece_index(id.try_into().ok()?)
                    })
                    .enumerate()
                {
                    if fo.check_piece(piece_id).is_err() {
                        return true;
                    }

                    #[allow(clippy::cast_possible_truncation)]
                    let progress = (self.metadata.lengths().total_length() as f64
                        / to_validate_count as f64
                        * (id + 1) as f64) as u64;
                    let progress = progress.min(self.metadata.lengths().total_length());
                    self.checked_bytes.store(progress, Ordering::Relaxed);
                }

                false
            })
            .await;

        if is_broken {
            warn!(
                id = ?self.shared.id,
                info_hash = ?self.shared.info_hash,
                "data corrupted, ignoring fastresume data"
            );
            if let Err(e) = bitv_factory.clear(self.shared.id.into()).await {
                warn!(id=?self.shared.id, info_hash = ?self.shared.info_hash, "error clearing bitfield: {e:#}");
            }
            self.checked_bytes.store(0, Ordering::Relaxed);
            return None;
        }

        Some(hp)
    }

    pub async fn check(&self) -> anyhow::Result<TorrentStatePaused> {
        let id: TorrentIdOrHash = self.shared.info_hash.into();
        let bitv_factory = self
            .shared
            .session
            .upgrade()
            .context("session is dead")?
            .bitv_factory
            .clone();
        let have_pieces = if self.previously_errored {
            if let Err(e) = bitv_factory.clear(id).await {
                warn!(id=?self.shared.id, info_hash = ?self.shared.info_hash, error=?e, "error clearing bitfield");
            }
            None
        } else {
            bitv_factory
                .load(id)
                .await
                .context("error loading have_pieces")?
        };

        let have_pieces = self.validate_fastresume(&*bitv_factory, have_pieces).await;

        let selected_pieces = compute_selected_pieces(
            self.metadata.lengths(),
            |idx| self.is_selected(idx),
            &self.metadata.file_infos,
        );

        let have_pieces = match have_pieces {
            Some(h) => h,
            None => {
                let trusted = if self.skip_check && !self.previously_errored {
                    self.shared
                        .spawner
                        .block_in_place_with_semaphore(|| self.trusted_pieces(&selected_pieces))
                        .await
                } else {
                    None
                };
                let have_pieces = match trusted {
                    Some(have_pieces) => {
                        info!("All selected files have their expected length, skipping the check");
                        self.checked_bytes
                            .store(self.metadata.lengths().total_length(), Ordering::Relaxed);
                        have_pieces
                    }
                    None => {
                        info!("Doing initial checksum validation, this might take a while...");
                        self.shared
                            .spawner
                            .block_in_place_with_semaphore(|| {
                                FileOps::new(
                                    &self.metadata.info,
                                    &self.files,
                                    &self.metadata.file_infos,
                                )
                                .initial_check(&self.checked_bytes, &self.pause_requested)
                            })
                            .await?
                    }
                };
                bitv_factory
                    .store_initial_check(id, have_pieces)
                    .await
                    .context("error storing initial check bitfield")?
            }
        };

        let chunk_tracker = ChunkTracker::new(
            have_pieces.into_dyn(),
            selected_pieces,
            *self.metadata.lengths(),
            &self.metadata.file_infos,
        )
        .context("error creating chunk tracker")?;

        let hns = chunk_tracker.get_hns();

        info!(
            torrent=?self.shared.id,
            "Initial check results: have {}, needed {}, total selected {}",
            SF::new(hns.have_bytes),
            SF::new(hns.needed_bytes),
            SF::new(hns.selected_bytes)
        );

        // Ensure file lengths are correct, and reopen read-only.
        self.shared
            .spawner
            .block_in_place_with_semaphore(|| {
                for (idx, fi) in self.metadata.file_infos.iter().enumerate() {
                    if self
                        .only_files
                        .as_ref()
                        .map(|v| v.contains(&idx))
                        .unwrap_or(true)
                    {
                        let now = Instant::now();
                        if fi.attrs.padding {
                            continue;
                        }
                        if let Err(err) = self.files.ensure_file_length(idx, fi.len) {
                            warn!(
                                id=?self.shared.id, info_hash = ?self.shared.info_hash,
                                "Error setting length for file {:?} to {}: {:#?}",
                                fi.relative_filename, fi.len, err
                            );
                        } else {
                            trace!(
                                "Set length for file {:?} to {} in {:?}",
                                fi.relative_filename,
                                SF::new(fi.len),
                                now.elapsed()
                            );
                        }
                    }
                }
                Ok::<_, anyhow::Error>(())
            })
            .await?;

        let paused = TorrentStatePaused {
            shared: self.shared.clone(),
            metadata: self.metadata.clone(),
            files: self.files.take()?,
            chunk_tracker,
            streams: Arc::new(Default::default()),
            carried_stats: self.carried_stats,
        };
        Ok(paused)
    }

    fn is_selected(&self, file_index: usize) -> bool {
        self.only_files
            .as_ref()
            .map(|o| o.contains(&file_index))
            .unwrap_or(true)
    }

    // For skip_initial_check: the selected pieces, if every selected file is on disk with its
    // expected length, except those that also lie in an unselected file that isn't. `None`
    // if a selected file is missing or has another length: then the files must be hashed.
    fn trusted_pieces(&self, selected_pieces: &BF) -> Option<BF> {
        let mut have = selected_pieces.clone();
        for (idx, fi) in self.metadata.file_infos.iter().enumerate() {
            if fi.attrs.padding {
                continue;
            }
            let len = match self.files.file_length(idx) {
                Ok(len) => len,
                Err(e) => {
                    warn!(file=?fi.relative_filename, "can't tell the length of the file, will check it: {e:#}");
                    None
                }
            };
            if len == Some(fi.len) {
                continue;
            }
            if self.is_selected(idx) {
                info!(file=?fi.relative_filename, expected=fi.len, actual=?len, "the file doesn't have its expected length, will do a full check");
                return None;
            }
            if let Some(pieces) = have.get_mut(fi.piece_range_usize()) {
                pieces.fill(false);
            }
        }
        Some(have)
    }
}
