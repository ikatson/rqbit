use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};

use anyhow::Context;

use itertools::Itertools;
use librqbit_core::lengths::Lengths;
use rand::Rng;
use size_format::SizeFormatterBinary as SF;
use tracing::{info, trace, warn};

use crate::{
    api::TorrentIdOrHash,
    bitv::BitV,
    bitv_factory::BitVFactory,
    chunk_tracker::ChunkTracker,
    file_ops::FileOps,
    type_aliases::{FileStorage, BF},
    FileInfos,
};

use super::{paused::TorrentStatePaused, ManagedTorrentShared, TorrentMetadata};

pub struct TorrentStateInitializing {
    pub(crate) files: FileStorage,
    pub(crate) shared: Arc<ManagedTorrentShared>,
    pub(crate) metadata: Arc<TorrentMetadata>,
    pub(crate) only_files: Option<Vec<usize>>,
    pub(crate) checked_bytes: AtomicU64,
    previously_errored: bool,
}

fn compute_selected_pieces(
    lengths: &Lengths,
    only_files: Option<&[usize]>,
    file_infos: &FileInfos,
) -> BF {
    let mut bf = BF::from_boxed_slice(vec![0u8; lengths.piece_bitfield_bytes()].into_boxed_slice());
    for (_, fi) in file_infos
        .iter()
        .enumerate()
        .filter(|(id, _)| only_files.map(|of| of.contains(id)).unwrap_or(true))
    {
        if let Some(r) = bf.get_mut(fi.piece_range_usize()) {
            r.fill(true);
        }
    }
    bf
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
            previously_errored,
        }
    }

    pub fn get_checked_bytes(&self) -> u64 {
        self.checked_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    async fn validate_fastresume(
        &self,
        bitv_factory: &dyn BitVFactory,
        have_pieces: Option<Box<dyn BitV>>,
    ) -> Option<Box<dyn BitV>> {
        let hp = have_pieces?;
        let actual = hp.as_bytes().len();
        let expected = self.metadata.lengths.piece_bitfield_bytes();
        if actual != expected {
            warn!(
                actual,
                expected,
                "the bitfield loaded isn't of correct length, ignoring it, will do full check"
            );
            return None;
        }

        let is_broken = self.shared.spawner.spawn_block_in_place(|| {
            let fo = crate::file_ops::FileOps::new(
                &self.metadata.info,
                &self.files,
                &self.metadata.file_infos,
                &self.metadata.lengths,
            );

            use rand::seq::SliceRandom;

            let mut to_validate = BF::from_boxed_slice(
                vec![0u8; self.metadata.lengths.piece_bitfield_bytes()].into_boxed_slice(),
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
            let mut queue = queue.iter_ones().collect_vec();
            queue.shuffle(&mut rand::rng());
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
                        .lengths
                        .validate_piece_index(id.try_into().ok()?)
                })
                .enumerate()
            {
                if fo.check_piece(piece_id).is_err() {
                    return true;
                }

                #[allow(clippy::cast_possible_truncation)]
                let progress = (self.metadata.lengths.total_length() as f64
                    / to_validate_count as f64
                    * (id + 1) as f64) as u64;
                let progress = progress.min(self.metadata.lengths.total_length());
                self.checked_bytes.store(progress, Ordering::Relaxed);
            }

            false
        });

        if is_broken {
            warn!("data corrupted, ignoring fastresume data");
            if let Err(e) = bitv_factory.clear(self.shared.id.into()).await {
                warn!(error=?e, "error clearing bitfield");
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
                warn!(error=?e, "error clearing bitfield");
            }
            None
        } else {
            bitv_factory
                .load(id)
                .await
                .context("error loading have_pieces")?
        };

        let have_pieces = self.validate_fastresume(&*bitv_factory, have_pieces).await;

        let have_pieces = match have_pieces {
            Some(h) => h,
            None => {
                info!("Doing initial checksum validation, this might take a while...");
                let have_pieces = self.shared.spawner.spawn_block_in_place(|| {
                    FileOps::new(
                        &self.metadata.info,
                        &self.files,
                        &self.metadata.file_infos,
                        &self.metadata.lengths,
                    )
                    .initial_check(&self.checked_bytes)
                })?;
                bitv_factory
                    .store_initial_check(id, have_pieces)
                    .await
                    .context("error storing initial check bitfield")?
            }
        };

        let selected_pieces = compute_selected_pieces(
            &self.metadata.lengths,
            self.only_files.as_deref(),
            &self.metadata.file_infos,
        );

        let chunk_tracker = ChunkTracker::new(
            have_pieces.into_dyn(),
            selected_pieces,
            self.metadata.lengths,
            &self.metadata.file_infos,
        )
        .context("error creating chunk tracker")?;

        let hns = chunk_tracker.get_hns();

        info!(
            "Initial check results: have {}, needed {}, total selected {}",
            SF::new(hns.have_bytes),
            SF::new(hns.needed_bytes),
            SF::new(hns.selected_bytes)
        );

        // Ensure file lenghts are correct, and reopen read-only.
        self.shared.spawner.spawn_block_in_place(|| {
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
        })?;

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
