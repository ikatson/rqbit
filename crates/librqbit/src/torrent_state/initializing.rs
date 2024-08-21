use std::{
    sync::{atomic::AtomicU64, Arc},
    time::Instant,
};

use anyhow::Context;

use librqbit_core::lengths::Lengths;
use size_format::SizeFormatterBinary as SF;
use tracing::{debug, info, warn};

use crate::{
    api::TorrentIdOrHash,
    bitv::BitV,
    bitv_factory::BitVFactory,
    chunk_tracker::ChunkTracker,
    file_ops::FileOps,
    type_aliases::{FileStorage, BF},
    FileInfos,
};

use super::{paused::TorrentStatePaused, ManagedTorrentShared};

pub struct TorrentStateInitializing {
    pub(crate) files: FileStorage,
    pub(crate) meta: Arc<ManagedTorrentShared>,
    pub(crate) only_files: Option<Vec<usize>>,
    pub(crate) checked_bytes: AtomicU64,
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
        .filter(|(id, _)| only_files.map(|of| of.contains(id)).unwrap_or(false))
    {
        if let Some(r) = bf.get_mut(fi.piece_range_usize()) {
            r.fill(true);
        }
    }
    bf
}

impl TorrentStateInitializing {
    pub fn new(
        meta: Arc<ManagedTorrentShared>,
        only_files: Option<Vec<usize>>,
        files: FileStorage,
    ) -> Self {
        Self {
            meta,
            only_files,
            files,
            checked_bytes: AtomicU64::new(0),
        }
    }

    pub fn get_checked_bytes(&self) -> u64 {
        self.checked_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub async fn check(
        &self,
        bitv_factory: Arc<dyn BitVFactory>,
    ) -> anyhow::Result<TorrentStatePaused> {
        let id: TorrentIdOrHash = self.meta.info_hash.into();
        let mut have_pieces = bitv_factory
            .load(id)
            .await
            .context("error loading have_pieces")?;
        if let Some(hp) = have_pieces.as_ref() {
            let actual = hp.as_bytes().len();
            let expected = self.meta.lengths.piece_bitfield_bytes();
            if actual != expected {
                warn!(
                    actual,
                    expected,
                    "the bitfield loaded isn't of correct length, ignoring it, will do full check"
                );
                have_pieces = None;
            }
        }
        let have_pieces = match have_pieces {
            Some(h) => h,
            None => {
                info!("Doing initial checksum validation, this might take a while...");
                let have_pieces = self.meta.spawner.spawn_block_in_place(|| {
                    FileOps::new(
                        &self.meta.info,
                        &self.files,
                        &self.meta.file_infos,
                        &self.meta.lengths,
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
            &self.meta.lengths,
            self.only_files.as_deref(),
            &self.meta.file_infos,
        );

        let chunk_tracker = ChunkTracker::new(
            have_pieces.into_dyn(),
            selected_pieces,
            self.meta.lengths,
            &self.meta.file_infos,
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
        self.meta.spawner.spawn_block_in_place(|| {
            for (idx, fi) in self.meta.file_infos.iter().enumerate() {
                if self
                    .only_files
                    .as_ref()
                    .map(|v| v.contains(&idx))
                    .unwrap_or(true)
                {
                    let now = Instant::now();
                    if let Err(err) = self.files.ensure_file_length(idx, fi.len) {
                        warn!(
                            "Error setting length for file {:?} to {}: {:#?}",
                            fi.relative_filename, fi.len, err
                        );
                    } else {
                        debug!(
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
            info: self.meta.clone(),
            files: self.files.take()?,
            chunk_tracker,
            streams: Arc::new(Default::default()),
        };
        Ok(paused)
    }
}
