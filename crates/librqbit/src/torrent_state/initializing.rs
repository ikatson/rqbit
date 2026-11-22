use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use librqbit_core::lengths::ValidPieceIndex;
use rand::{RngExt, seq::IteratorRandom};
use size_format::SizeFormatterBinary as SF;
use tokio_util::sync::CancellationToken;
use tracing::{info, trace, warn};

use crate::{
    api::TorrentIdOrHash,
    bitv::BitV,
    bitv_factory::BitVFactory,
    chunk_tracker::{ChunkTracker, compute_selected_pieces},
    file_ops::{FileOps, InitialCheckState},
    type_aliases::{BF, FileStorage},
};

const MAX_FASTRESUME_CHECKS: usize = 64;
const CHECK_SLICE_MAX_BYTES: usize = 4 * 1024 * 1024;
const CHECK_SLICE_MAX_DURATION: Duration = Duration::from_millis(20);
const ENSURE_LENGTH_FILES_PER_SLICE: usize = 64;

use super::{ManagedTorrentShared, TorrentMetadata, paused::TorrentStatePaused};

pub struct TorrentStateInitializing {
    pub(crate) files: FileStorage,
    pub(crate) shared: Arc<ManagedTorrentShared>,
    pub(crate) metadata: Arc<TorrentMetadata>,
    pub(crate) only_files: Option<Vec<usize>>,
    pub(crate) checked_bytes: AtomicU64,
    pause_requested: AtomicBool,
    check_running: AtomicBool,
    previously_errored: bool,
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
        }
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
        self: &Arc<Self>,
        bitv_factory: &dyn BitVFactory,
        have_pieces: Option<Box<dyn BitV>>,
        cancel_token: &CancellationToken,
    ) -> anyhow::Result<Option<Box<dyn BitV>>> {
        let Some(hp) = have_pieces else {
            return Ok(None);
        };
        let actual = hp.as_bytes().len();
        let expected = self.metadata.lengths().piece_bitfield_bytes();
        if actual != expected {
            warn!(
                actual,
                expected,
                "the bitfield loaded isn't of correct length, ignoring it, will do full check"
            );
            return Ok(None);
        }

        let pieces_to_validate: Vec<ValidPieceIndex> = {
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

            to_validate
                .iter_ones()
                .filter_map(|id| {
                    self.metadata
                        .lengths()
                        .validate_piece_index(id.try_into().ok()?)
                })
                .collect()
        };

        let to_validate_count = pieces_to_validate.len();
        let pieces_to_validate = Arc::new(pieces_to_validate);
        let mut cursor = 0usize;
        let mut read_buf = vec![0u8; 65536];
        let mut is_broken = false;

        while cursor < to_validate_count {
            if self.is_pause_requested() || cancel_token.is_cancelled() {
                bail!("initial check paused");
            }

            let init = self.clone();
            let pieces = pieces_to_validate.clone();
            let cancel = cancel_token.clone();
            let mut buf = std::mem::take(&mut read_buf);

            let (next_cursor, broken, returned_buf) = self
                .shared
                .spawner
                .spawn_blocking(move || -> anyhow::Result<(usize, bool, Vec<u8>)> {
                    let fo =
                        FileOps::new(&init.metadata.info, &init.files, &init.metadata.file_infos);
                    let slice_start = Instant::now();
                    let mut slice_bytes = 0usize;
                    let mut idx = cursor;

                    while idx < to_validate_count {
                        if init.is_pause_requested() || cancel.is_cancelled() {
                            bail!("initial check paused");
                        }

                        let piece_id = pieces[idx];
                        match fo.check_piece_with_buf_abortable(piece_id, &mut buf, || {
                            init.is_pause_requested() || cancel.is_cancelled()
                        }) {
                            Ok(true) => {}
                            Ok(false) => return Ok((idx + 1, true, buf)),
                            Err(err) => {
                                if init.is_pause_requested() || cancel.is_cancelled() {
                                    return Err(err);
                                }
                                return Ok((idx + 1, true, buf));
                            }
                        }

                        idx += 1;
                        slice_bytes =
                            slice_bytes.saturating_add(
                                init.metadata.lengths().piece_length(piece_id) as usize,
                            );

                        #[allow(clippy::cast_possible_truncation)]
                        let progress = (init.metadata.lengths().total_length() as f64
                            / to_validate_count as f64
                            * idx as f64) as u64;
                        let progress = progress.min(init.metadata.lengths().total_length());
                        init.checked_bytes.store(progress, Ordering::Relaxed);

                        if idx < to_validate_count
                            && (slice_bytes >= CHECK_SLICE_MAX_BYTES
                                || slice_start.elapsed() >= CHECK_SLICE_MAX_DURATION)
                        {
                            break;
                        }
                    }

                    Ok((idx, false, buf))
                })
                .await??;

            cursor = next_cursor;
            read_buf = returned_buf;
            if broken {
                is_broken = true;
                break;
            }
            if cursor < to_validate_count {
                tokio::task::yield_now().await;
            }
        }

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
            return Ok(None);
        }

        Ok(Some(hp))
    }

    pub async fn check(
        self: &Arc<Self>,
        cancel_token: &CancellationToken,
    ) -> anyhow::Result<TorrentStatePaused> {
        self.checked_bytes.store(0, Ordering::Relaxed);

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

        let have_pieces = self
            .validate_fastresume(&*bitv_factory, have_pieces, cancel_token)
            .await?;

        let have_pieces = match have_pieces {
            Some(h) => h,
            None => {
                info!("Doing initial checksum validation, this might take a while...");
                let mut check_state =
                    InitialCheckState::new(self.metadata.lengths(), &self.metadata.file_infos)?;
                loop {
                    if self.is_pause_requested() || cancel_token.is_cancelled() {
                        bail!("initial check paused");
                    }

                    let init = self.clone();
                    let cancel = cancel_token.clone();
                    let (next_state, done) = self
                        .shared
                        .spawner
                        .spawn_blocking(move || -> anyhow::Result<(InitialCheckState, bool)> {
                            let fo = FileOps::new(
                                &init.metadata.info,
                                &init.files,
                                &init.metadata.file_infos,
                            );
                            let done = fo.initial_check_step(
                                &mut check_state,
                                &init.checked_bytes,
                                || init.is_pause_requested() || cancel.is_cancelled(),
                                CHECK_SLICE_MAX_BYTES,
                                CHECK_SLICE_MAX_DURATION,
                            )?;
                            Ok((check_state, done))
                        })
                        .await??;

                    check_state = next_state;
                    if done {
                        break;
                    }
                    tokio::task::yield_now().await;
                }

                bitv_factory
                    .store_initial_check(id, check_state.into_have_pieces())
                    .await
                    .context("error storing initial check bitfield")?
            }
        };

        let selected_pieces = compute_selected_pieces(
            self.metadata.lengths(),
            |idx| {
                self.only_files
                    .as_ref()
                    .map(|o| o.contains(&idx))
                    .unwrap_or(true)
            },
            &self.metadata.file_infos,
        );

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

        // Ensure file lengths are correct in bounded slices so multi-file torrents don't pin threads.
        let total_files = self.metadata.file_infos.len();
        let mut next_file_idx = 0usize;
        while next_file_idx < total_files {
            if self.is_pause_requested() || cancel_token.is_cancelled() {
                bail!("initial check paused");
            }
            let init = self.clone();
            let cancel = cancel_token.clone();
            next_file_idx = self
                .shared
                .spawner
                .spawn_blocking(move || -> anyhow::Result<usize> {
                    let slice_start = Instant::now();
                    let mut processed_in_slice = 0usize;
                    let mut idx = next_file_idx;

                    while idx < total_files {
                        if init.is_pause_requested() || cancel.is_cancelled() {
                            bail!("initial check paused");
                        }
                        let fi = &init.metadata.file_infos[idx];
                        idx += 1;

                        let is_selected = init
                            .only_files
                            .as_ref()
                            .map(|v| v.contains(&(idx - 1)))
                            .unwrap_or(true);
                        if !is_selected || fi.attrs.padding {
                            continue;
                        }

                        let now = Instant::now();
                        if let Err(err) = init.files.ensure_file_length(idx - 1, fi.len) {
                            warn!(
                                id=?init.shared.id, info_hash = ?init.shared.info_hash,
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

                        processed_in_slice += 1;
                        if idx < total_files
                            && (processed_in_slice >= ENSURE_LENGTH_FILES_PER_SLICE
                                || slice_start.elapsed() >= CHECK_SLICE_MAX_DURATION)
                        {
                            break;
                        }
                    }

                    Ok(idx)
                })
                .await??;

            if next_file_idx < total_files {
                tokio::task::yield_now().await;
            }
        }

        let paused = TorrentStatePaused {
            shared: self.shared.clone(),
            metadata: self.metadata.clone(),
            files: self.files.take()?,
            chunk_tracker,
            streams: Arc::new(Default::default()),
        };
        Ok(paused)
    }
}
