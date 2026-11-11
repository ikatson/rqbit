use std::{
    collections::VecDeque,
    future::Future,
    io::SeekFrom,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Poll, Waker},
    time::Instant,
};

use anyhow::{Context, bail};
use dashmap::DashMap;

use librqbit_core::lengths::{CurrentPiece, Lengths, ValidPieceIndex};
use tokio::{
    io::{AsyncRead, AsyncSeek},
    sync::OwnedSemaphorePermit,
};
use tracing::{debug, trace};

use crate::{ManagedTorrent, ManagedTorrentState, chunk_tracker::ChunkTracker};

use super::{ManagedTorrentHandle, ManagedTorrentLocked, TorrentMetadata};

type StreamId = usize;

// 32 mb lookahead by default.
const PER_STREAM_BUF_DEFAULT: u64 = 32 * 1024 * 1024;

struct StreamState {
    file_id: usize,
    file_len: u64,
    file_abs_offset: u64,
    position: u64,
    waker: Option<Waker>,
}

impl StreamState {
    fn current_piece(&self, lengths: &Lengths) -> Option<CurrentPiece> {
        lengths.compute_current_piece(self.position, self.file_abs_offset)
    }

    fn queue<'a>(&self, lengths: &'a Lengths) -> impl Iterator<Item = ValidPieceIndex> + use<'a> {
        let start = self.file_abs_offset + self.position;
        let end = (start + PER_STREAM_BUF_DEFAULT).min(self.file_abs_offset + self.file_len);
        let dpl = lengths.default_piece_length();
        let start_id = (start / dpl as u64).try_into().unwrap();
        let end_id = end.div_ceil(dpl as u64).try_into().unwrap();
        (start_id..end_id).filter_map(|i| lengths.validate_piece_index(i))
    }
}

#[derive(Default)]
pub(crate) struct TorrentStreams {
    next_stream_id: AtomicUsize,
    streams: DashMap<StreamId, StreamState>,
}

impl TorrentStreams {
    fn next_id(&self) -> usize {
        self.next_stream_id.fetch_add(1, Ordering::Relaxed)
    }

    fn register_waker(&self, stream_id: StreamId, waker: Waker) {
        if let Some(mut s) = self.streams.get_mut(&stream_id) {
            let vm = s.value_mut();
            vm.waker = Some(waker);
        }
    }

    // Interleave 1st, 2nd etc pieces from each active stream in turn until they get 1/10th of the file .
    pub(crate) fn iter_next_pieces<'a>(
        &'a self,
        lengths: &'a Lengths,
    ) -> impl Iterator<Item = ValidPieceIndex> + 'a {
        struct Interleave<I> {
            all: VecDeque<I>,
        }

        impl<I: Iterator<Item = ValidPieceIndex>> Iterator for Interleave<I> {
            type Item = ValidPieceIndex;

            fn next(&mut self) -> Option<Self::Item> {
                while let Some(mut it) = self.all.pop_front() {
                    if let Some(piece) = it.next() {
                        self.all.push_back(it);
                        return Some(piece);
                    }
                }
                None
            }
        }

        let mut all: Vec<_> = self.streams.iter().map(|s| s.queue(lengths)).collect();

        // Shuffle to decrease determinism and make queueing fairer.
        use rand::seq::SliceRandom;
        all.shuffle(&mut rand::rng());

        Interleave { all: all.into() }
    }

    pub(crate) fn wake_streams_on_piece_completed(
        &self,
        piece_id: ValidPieceIndex,
        lengths: &Lengths,
    ) {
        for mut w in self.streams.iter_mut() {
            if w.value().current_piece(lengths).map(|p| p.id) == Some(piece_id)
                && let Some(waker) = w.value_mut().waker.take()
            {
                debug!(
                    stream_id = *w.key(),
                    piece_id = piece_id.get(),
                    "waking stream"
                );
                waker.wake();
            }
        }
    }

    // Wakes every stream waiting for a piece. A check that just ended may have found pieces
    // that no download will complete (and so wake their streams) any more.
    pub(crate) fn wake_all(&self) {
        for mut w in self.streams.iter_mut() {
            if let Some(waker) = w.value_mut().waker.take() {
                waker.wake();
            }
        }
    }

    fn drop_stream(&self, stream_id: StreamId) -> Option<StreamState> {
        debug!(stream_id, "dropping stream");
        self.streams.remove(&stream_id).map(|s| s.1)
    }

    pub(crate) fn streamed_file_ids(&self) -> impl Iterator<Item = usize> + '_ {
        self.streams.iter().map(|s| s.value().file_id)
    }
}

pub struct FileStream {
    torrent: ManagedTorrentHandle,
    metadata: Arc<TorrentMetadata>,
    streams: Arc<TorrentStreams>,
    stream_id: usize,
    file_id: usize,
    position: u64,

    // file params
    file_len: u64,
    file_torrent_abs_offset: u64,

    // Set while a storage operation has the storage (see `storage_busy`): the read waits for
    // it to end.
    storage_wait: Option<StorageWait>,

    _blocking_permit: OwnedSemaphorePermit,
}

type StorageWait = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + Sync>>;

// What one try at a stream read found.
enum StreamRead {
    // The bytes were read.
    Done,
    // The piece isn't there; the stream is woken once it is.
    MissingPiece,
    // The storage can't be read now (see `storage_busy`).
    StorageBusy,
}

// Whether the storage is out of a stream's reach for now: a storage operation (move, rename,
// recheck) has it, or the torrent checks its files (or waits to). Fails in a state the torrent
// doesn't leave on its own.
fn storage_busy(g: &ManagedTorrentLocked) -> anyhow::Result<bool> {
    match &g.state {
        ManagedTorrentState::Live(_) => Ok(false),
        ManagedTorrentState::Paused(_) => Ok(g.storage_on_loan),
        ManagedTorrentState::Initializing(_) => Ok(true),
        ManagedTorrentState::Error(e) => bail!("the torrent stopped with an error: {e:#}"),
        ManagedTorrentState::None => bail!("bug: torrent is in empty state"),
    }
}

// Waits until a stream can read the storage of `torrent` again: until the storage operation
// or the check that has it ends. Fails if the torrent is deleted or the session stops
// meanwhile, or if the torrent ends up in error.
async fn wait_storage_readable(torrent: ManagedTorrentHandle) -> anyhow::Result<()> {
    loop {
        // Registered before looking, so that a change right after the look still wakes us.
        let notified = torrent.state_change_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let busy = storage_busy(&torrent.locked.read())?;
        if !busy {
            return Ok(());
        }
        tokio::select! {
            _ = notified => {}
            _ = torrent.storage_op_cancel.cancelled() => {
                bail!("the torrent was deleted or the session stopped")
            }
        }
    }
}

macro_rules! map_io_err {
    ($e:expr) => {
        $e.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
    };
}

macro_rules! poll_try_io {
    ($e:expr) => {{
        let e = map_io_err!($e);
        match e {
            Ok(r) => r,
            Err(e) => {
                debug!("stream error {e:#}");
                return Poll::Ready(Err(e));
            }
        }
    }};
}

impl AsyncRead for FileStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        tbuf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        // if the file is over, return 0
        if this.position == this.file_len {
            debug!(
                stream_id = this.stream_id,
                file_id = this.file_id,
                "stream completed, EOF"
            );
            return Poll::Ready(Ok(()));
        }

        let bytes_read = loop {
            // A storage operation had the storage: once it's over, look again, as the piece
            // may be gone (a recheck found it damaged) and the file elsewhere (moved, renamed).
            if let Some(wait) = this.storage_wait.as_mut() {
                let waited = std::task::ready!(wait.as_mut().poll(cx));
                this.storage_wait = None;
                poll_try_io!(waited);
            }

            let current = poll_try_io!(
                this.metadata
                    .lengths()
                    .compute_current_piece(this.position, this.file_torrent_abs_offset)
                    .context("invalid position")
            );

            let buf = tbuf.initialize_unfilled();
            let file_remaining = this.file_len - this.position;
            let bytes_to_read: usize = poll_try_io!(
                (buf.len() as u64)
                    .min(current.piece_remaining as u64)
                    .min(file_remaining)
                    .try_into()
            );
            let buf = &mut buf[..bytes_to_read];

            let start = Instant::now();
            let read = poll_try_io!(this.torrent.shared.spawner.block_in_place(|| {
                this.torrent
                    .stream_read(this.file_id, this.position, current.id, buf, || {
                        this.streams
                            .register_waker(this.stream_id, cx.waker().clone())
                    })
            }));
            match read {
                StreamRead::Done => {
                    trace!(
                        buflen = bytes_to_read,
                        stream_id = this.stream_id,
                        file_id = this.file_id,
                        read_time = ?start.elapsed(),
                        "will write bytes"
                    );
                    break bytes_to_read;
                }
                StreamRead::MissingPiece => {
                    debug!(stream_id = this.stream_id, file_id = this.file_id, piece_id = %current.id, "poll pending, not have");
                    return Poll::Pending;
                }
                StreamRead::StorageBusy => {
                    debug!(
                        stream_id = this.stream_id,
                        file_id = this.file_id,
                        "storage busy, waiting for it"
                    );
                    this.storage_wait = Some(Box::pin(wait_storage_readable(this.torrent.clone())));
                }
            }
        };

        this.advance(bytes_read as u64);
        tbuf.advance(bytes_read);

        Poll::Ready(Ok(()))
    }
}

impl AsyncSeek for FileStream {
    fn start_seek(
        mut self: std::pin::Pin<&mut Self>,
        position: std::io::SeekFrom,
    ) -> std::io::Result<()> {
        let end_i64 = map_io_err!(TryInto::<i64>::try_into(self.file_len))?;
        let new_pos: i64 = match position {
            SeekFrom::Start(s) => map_io_err!(s.try_into())?,
            SeekFrom::End(e) => map_io_err!(TryInto::<i64>::try_into(self.file_len))? + e,
            SeekFrom::Current(o) => map_io_err!(TryInto::<i64>::try_into(self.position))? + o,
        };

        if new_pos < 0 || new_pos > end_i64 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                anyhow::anyhow!("invalid seek"),
            ));
        }

        self.as_mut().set_position(map_io_err!(new_pos.try_into())?);
        debug!(stream_id = self.stream_id, position = self.position, "seek");
        Ok(())
    }

    fn poll_complete(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<u64>> {
        Poll::Ready(Ok(self.position))
    }
}

impl Drop for FileStream {
    fn drop(&mut self) {
        self.streams.drop_stream(self.stream_id);
    }
}

impl ManagedTorrent {
    // One try at a stream's read of `buf.len()` bytes of file `file_id` at `offset`, inside
    // `piece`. The state lock is held from looking at the piece to the end of the read, so
    // that neither can a storage operation take the storage nor a recheck start in between.
    // `on_missing` (registering the stream's waker) runs under the lock that marking the piece
    // present takes, so that the wake-up can't be missed.
    fn stream_read(
        &self,
        file_id: usize,
        offset: u64,
        piece: ValidPieceIndex,
        buf: &mut [u8],
        on_missing: impl FnOnce(),
    ) -> anyhow::Result<StreamRead> {
        let g = self.locked.read();
        if storage_busy(&g)? {
            return Ok(StreamRead::StorageBusy);
        }
        let files = match &g.state {
            ManagedTorrentState::Live(l) => {
                let locked = l.lock_read("stream_read");
                let chunks = locked.get_chunks().context("error getting chunks")?;
                if !have_piece(chunks, piece) {
                    on_missing();
                    return Ok(StreamRead::MissingPiece);
                }
                &*l.files
            }
            ManagedTorrentState::Paused(p) => {
                if !have_piece(&p.chunk_tracker, piece) {
                    on_missing();
                    return Ok(StreamRead::MissingPiece);
                }
                &*p.files
            }
            s => bail!("bug: reading a stream in state {}", s.name()),
        };
        files.pread_exact(file_id, offset, buf)?;
        Ok(StreamRead::Done)
    }

    fn streams(&self) -> anyhow::Result<Arc<TorrentStreams>> {
        self.with_state(|s| match s {
            crate::ManagedTorrentState::Paused(p) => Ok(p.streams.clone()),
            crate::ManagedTorrentState::Live(l) => Ok(l.streams.clone()),
            s => anyhow::bail!("streams: invalid state {}", s.name()),
        })
    }

    fn maybe_reconnect_needed_peers_for_file(&self, file_id: usize) -> bool {
        // If we have the full file, don't bother.
        if self.is_file_finished(file_id) {
            return false;
        }
        self.with_state(|state| {
            if let crate::ManagedTorrentState::Live(l) = &state {
                l.reconnect_all_not_needed_peers();
            }
        });
        true
    }

    fn is_file_finished(&self, file_id: usize) -> bool {
        let metadata = self.metadata.load();
        let metadata = match metadata.as_ref() {
            Some(r) => r,
            None => return false,
        };
        // TODO: would be nice to remove locking
        self.with_chunk_tracker(|ct| ct.is_file_finished(&metadata.file_infos[file_id]))
            .unwrap_or(false)
    }

    pub async fn stream(self: Arc<Self>, file_id: usize) -> anyhow::Result<FileStream> {
        // Streams need live or paused state, so wait out the initial check
        // of existing data instead of failing while it runs.
        self.wait_until_initialized().await?;
        let metadata = self
            .metadata
            .load_full()
            .context("torrent metadata is not resolved")?;
        let fi = metadata.file_infos.get(file_id).context("invalid file")?;
        let (fd_len, fd_offset) = (fi.len, fi.offset_in_torrent);
        let streams = self.streams()?;
        let blocking_permit = self.shared().spawner.semaphore().acquire_owned().await?;
        let s = FileStream {
            stream_id: streams.next_id(),
            streams: streams.clone(),
            file_id,
            position: 0,

            file_len: fd_len,
            file_torrent_abs_offset: fd_offset,
            storage_wait: None,
            _blocking_permit: blocking_permit,
            torrent: self,
            metadata,
        };
        s.torrent.maybe_reconnect_needed_peers_for_file(file_id);
        streams.streams.insert(
            s.stream_id,
            StreamState {
                file_id,
                position: 0,
                waker: None,
                file_len: fd_len,
                file_abs_offset: fd_offset,
            },
        );

        debug!(stream_id = s.stream_id, file_id, "started stream");

        Ok(s)
    }
}

fn have_piece(chunks: &ChunkTracker, piece: ValidPieceIndex) -> bool {
    chunks
        .get_have_pieces()
        .as_slice()
        .get(piece.get() as usize)
        .is_some_and(|b| *b)
}

impl FileStream {
    pub fn position(&self) -> u64 {
        self.position
    }

    fn advance(&mut self, diff: u64) {
        self.set_position(self.position + diff)
    }

    fn set_position(&mut self, new_pos: u64) {
        self.position = new_pos;
        self.streams
            .streams
            .get_mut(&self.stream_id)
            .unwrap()
            .value_mut()
            .position = new_pos;
    }

    pub fn len(&self) -> u64 {
        self.file_len
    }
}
