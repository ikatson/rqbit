use std::{
    collections::VecDeque,
    io::{Read, Seek, SeekFrom},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Poll, Waker},
};

use anyhow::Context;
use dashmap::DashMap;
use librqbit_core::lengths::{Lengths, ValidPieceIndex};
use tokio::io::{AsyncRead, AsyncSeek};
use tracing::{debug, trace};

use crate::{opened_file::OpenedFile, ManagedTorrent};

use super::ManagedTorrentHandle;

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
        compute_current_piece(lengths, self.position, self.file_abs_offset)
    }

    fn queue<'a>(&self, lengths: &'a Lengths) -> impl Iterator<Item = ValidPieceIndex> + 'a {
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

struct CurrentPiece {
    id: ValidPieceIndex,
    piece_remaining: u32,
}

fn compute_current_piece(
    lengths: &Lengths,
    file_pos: u64,
    file_torrent_abs_offset: u64,
) -> Option<CurrentPiece> {
    let dpl = lengths.default_piece_length();

    let abs_pos = file_torrent_abs_offset + file_pos;
    let piece_id = abs_pos / dpl as u64;
    let piece_id: u32 = piece_id.try_into().ok()?;

    let piece_id = lengths.validate_piece_index(piece_id)?;
    let piece_len = lengths.piece_length(piece_id);
    Some(CurrentPiece {
        id: piece_id,
        piece_remaining: (piece_len as u64 - (abs_pos % dpl as u64))
            .try_into()
            .ok()?,
    })
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

        let all = self.streams.iter().map(|s| s.queue(lengths)).collect();
        Interleave { all }
    }

    pub(crate) fn wake_streams_on_piece_completed(
        &self,
        piece_id: ValidPieceIndex,
        lengths: &Lengths,
    ) {
        for mut w in self.streams.iter_mut() {
            if w.value().current_piece(lengths).map(|p| p.id) == Some(piece_id) {
                if let Some(waker) = w.value_mut().waker.take() {
                    trace!(
                        stream_id = *w.key(),
                        piece_id = piece_id.get(),
                        "waking stream"
                    );
                    waker.wake();
                }
            }
        }
    }

    fn drop_stream(&self, stream_id: StreamId) -> Option<StreamState> {
        trace!(stream_id, "dropping stream");
        self.streams.remove(&stream_id).map(|s| s.1)
    }

    pub(crate) fn streamed_file_ids(&self) -> impl Iterator<Item = usize> + '_ {
        self.streams.iter().map(|s| s.value().file_id)
    }
}

pub struct FileStream {
    torrent: ManagedTorrentHandle,
    streams: Arc<TorrentStreams>,
    stream_id: usize,
    file_id: usize,
    position: u64,

    // file params
    file_len: u64,
    file_torrent_abs_offset: u64,
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
                debug!("stream error {e:?}");
                return Poll::Ready(Err(e));
            }
        }
    }};
}

impl AsyncRead for FileStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        tbuf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // if the file is over, return 0
        if self.position == self.file_len {
            trace!(
                stream_id = self.stream_id,
                file_id = self.file_id,
                "stream completed, EOF"
            );
            return Poll::Ready(Ok(()));
        }

        let current = poll_try_io!(compute_current_piece(
            &self.torrent.info().lengths,
            self.position,
            self.file_torrent_abs_offset
        )
        .context("invalid position"));

        // if the piece is not there, register to wake when it is
        // check if we have the piece for real
        let have = poll_try_io!(self.torrent.with_chunk_tracker(|ct| {
            let have = ct.get_have_pieces()[current.id.get() as usize];
            if !have {
                self.streams
                    .register_waker(self.stream_id, cx.waker().clone());
            }
            have
        }));
        if !have {
            trace!(stream_id = self.stream_id, file_id = self.file_id, piece_id = %current.id, "poll pending, not have");
            return Poll::Pending;
        }

        // actually stream the piece
        let buf = tbuf.initialize_unfilled();
        let file_remaining = self.file_len - self.position;
        let bytes_to_read: usize = poll_try_io!((buf.len() as u64)
            .min(current.piece_remaining as u64)
            .min(file_remaining)
            .try_into());

        let buf = &mut buf[..bytes_to_read];
        trace!(
            buflen = buf.len(),
            stream_id = self.stream_id,
            file_id = self.file_id,
            "will write bytes"
        );

        poll_try_io!(poll_try_io!(self.torrent.with_opened_file(
            self.file_id,
            |fd| {
                let mut g = fd.file.lock();
                g.seek(SeekFrom::Start(self.position))?;
                g.read_exact(buf)?;
                Ok::<_, anyhow::Error>(())
            }
        )));

        self.as_mut().position += buf.len() as u64;
        tbuf.advance(bytes_to_read);
        self.streams
            .streams
            .get_mut(&self.stream_id)
            .unwrap()
            .value_mut()
            .position = self.position;

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

        self.as_mut().position = map_io_err!(new_pos.try_into())?;
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
    fn with_opened_file<F, R>(&self, file_id: usize, f: F) -> anyhow::Result<R>
    where
        F: FnOnce(&OpenedFile) -> R,
    {
        self.with_state(|s| {
            let files = match s {
                crate::ManagedTorrentState::Paused(p) => &p.files,
                crate::ManagedTorrentState::Live(l) => &l.files,
                _ => anyhow::bail!("invalid state"),
            };
            let fd = files.get(file_id).context("invalid file")?;
            Ok(f(fd))
        })
    }

    fn streams(&self) -> anyhow::Result<Arc<TorrentStreams>> {
        self.with_state(|s| match s {
            crate::ManagedTorrentState::Paused(p) => Ok(p.streams.clone()),
            crate::ManagedTorrentState::Live(l) => Ok(l.streams.clone()),
            _ => anyhow::bail!("invalid state"),
        })
    }

    fn maybe_reconnect_needed_peers_for_file(&self, file_id: usize) -> bool {
        // If we have the full file, don't bother.
        if let Ok(true) = self.with_opened_file(file_id, |f| f.approx_is_finished()) {
            return false;
        }
        self.with_state(|state| {
            if let crate::ManagedTorrentState::Live(l) = &state {
                l.reconnect_all_not_needed_peers();
            }
        });
        true
    }

    pub fn stream(self: Arc<Self>, file_id: usize) -> anyhow::Result<FileStream> {
        let (fd_len, fd_offset) =
            self.with_opened_file(file_id, |fd| (fd.len, fd.offset_in_torrent))?;
        let streams = self.streams()?;
        let s = FileStream {
            stream_id: streams.next_id(),
            streams: streams.clone(),
            file_id,
            position: 0,

            file_len: fd_len,
            file_torrent_abs_offset: fd_offset,
            torrent: self,
        };
        if s.torrent.maybe_reconnect_needed_peers_for_file(file_id) {
            s.torrent
                .with_opened_file(file_id, |fd| fd.reopen(false))??;
        }
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

        Ok(s)
    }
}

impl FileStream {
    pub fn position(&self) -> u64 {
        self.position
    }

    pub fn len(&self) -> u64 {
        self.file_len
    }
}
