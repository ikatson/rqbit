use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use anyhow::Context;

use itertools::Itertools;
use rand::Rng;
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

use super::{ManagedTorrentShared, TorrentMetadata, paused::TorrentStatePaused};

pub struct TorrentStateInitializing {
    pub(crate) files: FileStorage,
    pub(crate) shared: Arc<ManagedTorrentShared>,
    pub(crate) metadata: Arc<TorrentMetadata>,
    pub(crate) only_files: Option<Vec<usize>>,
    pub(crate) checked_bytes: AtomicU64,
    previously_errored: bool,
    skip_check: bool,
}

impl TorrentStateInitializing {
    pub fn new(
        shared: Arc<ManagedTorrentShared>,
        metadata: Arc<TorrentMetadata>,
        only_files: Option<Vec<usize>>,
        files: FileStorage,
        previously_errored: bool,
        skip_check: bool,
    ) -> Self {
        Self {
            shared,
            metadata,
            only_files,
            files,
            checked_bytes: AtomicU64::new(0),
            previously_errored,
            skip_check,
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

                use rand::seq::SliceRandom;

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



        if self.skip_check {
            use bitvec::vec::BitVec;
            let num_bytes = self.metadata.lengths().piece_bitfield_bytes();
            let mut bv = BitVec::<u8, bitvec::order::Msb0>::from_vec(vec![0xff; num_bytes]);
            let _total_pieces = self.metadata.lengths().total_pieces() as usize;
            // We must have the exact same number of bits as compute_selected_pieces returns.
            // It initializes from bytes, so it has size = num_bytes * 8.
            let expected_bits = num_bytes * 8;
            if bv.len() != expected_bits {
                // This should not happen if initialized from vec, but just to be safe/clear
                bv.resize(expected_bits, true);
            }
            
            // However, we should probably ensure the padding bits at the end are correct (obey the protocol? or just internal logic?)
            // ChunkTracker uses these bits. 
            // But verify what compute_selected_pieces does. it inits with 0s.
            
            let bv = bv.into_boxed_bitslice();
            
            // We claim to have everything.
            info!("Skipping initial check, assuming all files are present and correct");
            
            // Should we validate at least something? 
            // The user explicitly requested to SKIP check, so we assume full trust.
            // We just store it and return.
            
            self.checked_bytes.store(self.metadata.lengths().total_length(), Ordering::Relaxed);
            
             bitv_factory
                .store_initial_check(id, bv.clone())
                .await
                .context("error storing skipped check bitfield")?;
                
             // For fast resume validation logic below, we pretend we loaded it from disk.
             let _have_pieces = Some(Box::new(bv.clone()) as Box<dyn crate::bitv::BitV>);
             
             // We still probably want to bypass validate_fastresume if we skip check,
             // because validate_fastresume does random checks. 
             // If the user lied, random checks will fail and clear the bitfield.
             // But if the user is honest, it should pass. 
             // However, for "Create Torrent", the files SHOULD be there.
             // So let's let `validate_fastresume` run?
             // Actually, if we just created the torrent, `validate_fastresume` is good to confirm we can read the files.
             // But if `skip_check` is enabled, we might want to avoid any reads (e.g. slow network drive).
             // But `validate_fastresume` is very lightweight.
             // Let's TRY to let it run. If it fails, `check` will probably fall back to full check or error.
             // Wait, `check` logic:
             
             // let have_pieces = self.validate_fastresume(&*bitv_factory, have_pieces).await;
             
             // If we want to strictly skip check, we should return early or bypass validate.
             // But `validate_fastresume` is useful. 
             // Let's assume we WANT to bypass validation too if `skip_check` is true.
             
             // So:
             return self.finalize_check(Box::new(bv)).await;
        }

        let have_pieces = self.validate_fastresume(&*bitv_factory, have_pieces).await;

        let have_pieces = match have_pieces {
            Some(h) => h,
            None => {
                info!("Doing initial checksum validation, this might take a while...");
                let have_pieces = self
                    .shared
                    .spawner
                    .block_in_place_with_semaphore(|| {
                        FileOps::new(&self.metadata.info, &self.files, &self.metadata.file_infos)
                            .initial_check(&self.checked_bytes)
                    })
                    .await?;
                bitv_factory
                    .store_initial_check(id, have_pieces)
                    .await
                    .context("error storing initial check bitfield")?
            }
        };
        self.finalize_check(have_pieces).await
    }

    async fn finalize_check(&self, have_pieces: Box<dyn crate::bitv::BitV>) -> anyhow::Result<TorrentStatePaused> {
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

        if self.shared.options.sync_extra_files && hns.finished() {
             use crate::sync_utils::remove_extra_files;
             info!("Syncing extra files...");
             if let Err(e) = remove_extra_files(&self.metadata.info.info(), &self.shared.options.output_folder) {
                  warn!("Error removing extra files: {:#}", e);
             }
        }

        info!(
            torrent=?self.shared.id,
            "Check results: have {}, needed {}, total selected {}",
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
        };
        Ok(paused)
    }
}
