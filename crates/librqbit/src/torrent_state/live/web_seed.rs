//! What the web seed workers ([`crate::webseed`]) need from the live state: they reserve pieces
//! through the same piece tracker and picker the peers use, under a handle of their own.

use std::{num::NonZeroU32, time::Duration};

use librqbit_core::lengths::{Lengths, ValidPieceIndex};
use tokio::sync::Notify;
use tracing::{debug, trace};

use crate::{
    file_info::FileInfo,
    piece_tracker::{AcquireRequest, AcquireResult},
    type_aliases::PeerHandle,
};

use super::{TorrentStateLive, TorrentStateLocked};

impl TorrentStateLive {
    pub(crate) fn lengths(&self) -> &Lengths {
        &self.lengths
    }

    pub(crate) fn file_infos(&self) -> &[FileInfo] {
        &self.metadata.file_infos
    }

    /// Woken when pieces go back to the queue or new ones are wanted.
    pub(crate) fn new_pieces_notify(&self) -> &Notify {
        &self.new_pieces_notify
    }

    /// Reserves pieces for the web seed `seed`: the first one through the same path peers take
    /// (streams, the file order, stealing from a much slower source when `avg_piece_time` is
    /// known), then the queued pieces that follow it, as long as they lie in the file the run
    /// ends in, up to `max_pieces` in all.
    /// `has_piece` says which pieces the web seed can serve. Empty when there is nothing to do.
    pub(crate) fn web_seed_reserve(
        &self,
        seed: PeerHandle,
        avg_piece_time: Option<Duration>,
        has_piece: impl Fn(ValidPieceIndex) -> bool,
        max_pieces: usize,
    ) -> crate::Result<Vec<ValidPieceIndex>> {
        if self.is_finished_and_no_active_streams() {
            return Ok(Vec::new());
        }
        let mut steal_info = None;
        // A piece whose expected hash is not known yet (BEP 52: its file's piece layer is still
        // being fetched from peers) could not be checked, so it is not downloaded.
        let has_piece = |p: ValidPieceIndex| self.metadata.info.can_verify_piece(p) && has_piece(p);
        let run = {
            let mut g = self.lock_write("web_seed_reserve");
            let TorrentStateLocked {
                pieces,
                file_priorities,
                ..
            } = &mut **g;
            let pieces = pieces.as_mut().ok_or(crate::Error::ChunkTrackerEmpty)?;
            let first = match pieces.acquire_piece(AcquireRequest {
                peer: seed,
                peer_avg_time: avg_piece_time,
                priority_pieces: self.streams.iter_next_pieces(&self.lengths),
                file_priorities,
                file_infos: &self.metadata.file_infos,
                peer_has_piece: &has_piece,
                can_steal: |p| self.per_piece_locks[p.get_usize()].try_write().is_some(),
            }) {
                AcquireResult::Reserved(piece) => piece,
                AcquireResult::Stolen { piece, from_peer } => {
                    debug!(?seed, "web seed stole piece {piece} from {from_peer}");
                    steal_info = Some((from_peer, piece));
                    piece
                }
                AcquireResult::NoneAvailable => return Ok(Vec::new()),
            };
            let mut run = vec![first];
            if steal_info.is_none() {
                let file_end = self.file_end_containing(
                    self.lengths.piece_offset(first) + u64::from(self.lengths.piece_length(first))
                        - 1,
                );
                while run.len() < max_pieces {
                    let Some(next) = run
                        .last()
                        .and_then(|p| p.get().checked_add(1))
                        .and_then(|p| self.lengths.validate_piece_index(p))
                    else {
                        break;
                    };
                    let end = self.lengths.piece_offset(next)
                        + u64::from(self.lengths.piece_length(next));
                    if file_end.is_none_or(|file_end| end > file_end)
                        || !has_piece(next)
                        || !pieces.reserve_queued(next, seed)
                    {
                        break;
                    }
                    run.push(next);
                }
            }
            run
        };
        // Outside of the state lock, like the peers do.
        if let Some((from_peer, piece)) = steal_info {
            self.peers.on_steal(from_peer, seed, piece);
        }
        trace!(?seed, ?run, "web seed reserved pieces");
        Ok(run)
    }

    /// One past the last byte of the non-padding file that holds the torrent byte `offset`.
    fn file_end_containing(&self, offset: u64) -> Option<u64> {
        self.metadata
            .file_infos
            .iter()
            .find(|f| {
                !f.attrs.padding
                    && f.offset_in_torrent <= offset
                    && offset - f.offset_in_torrent < f.len
            })
            .map(|f| f.offset_in_torrent + f.len)
    }

    /// Whether `seed` is still the one downloading `piece` (peers may have stolen it, or it may
    /// not be wanted any more).
    pub(crate) fn web_seed_owns(&self, seed: PeerHandle, piece: ValidPieceIndex) -> bool {
        self.lock_read("web_seed_owns")
            .get_pieces()
            .ok()
            .and_then(|p| p.get_inflight(piece))
            .is_some_and(|i| i.peer == seed)
    }

    /// Gives the pieces `seed` is downloading back to the queue.
    pub(crate) fn web_seed_release(&self, seed: PeerHandle) -> crate::Result<()> {
        let released = self
            .lock_write("web_seed_release")
            .get_pieces_mut()?
            .release_pieces_owned_by(seed);
        if released > 0 {
            trace!(?seed, released, "web seed gave pieces back");
            self.new_pieces_notify.notify_waiters();
        }
        Ok(())
    }

    /// Waits for both the torrent's and the session's download limits to allow `len` bytes.
    pub(crate) async fn wait_for_web_seed_download_rate(&self, len: u32) -> crate::Result<()> {
        let Some(len) = NonZeroU32::new(len) else {
            return Ok(());
        };
        self.ratelimits.prepare_for_download(len).await?;
        if let Some(session) = self.shared.session.upgrade() {
            session.ratelimits.prepare_for_download(len).await?;
        }
        Ok(())
    }
}
