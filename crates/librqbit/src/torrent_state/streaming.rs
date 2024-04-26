use std::{
    io::{Read, Seek, SeekFrom},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Poll, Waker},
};

use anyhow::Context;
use dashmap::DashMap;
use itertools::Itertools;
use librqbit_core::lengths::{Lengths, ValidPieceIndex};
use tokio::io::{AsyncRead, AsyncSeek};
use tracing::{debug, trace};

use crate::{opened_file::OpenedFile, ManagedTorrent};

use super::ManagedTorrentHandle;

type StreamId = usize;

// Buffer either 1/10th of the file forward.
const PER_STREAM_BUF_PART: u64 = 10;
// Or 32 mb, whichever is larger
const PER_STREAM_BUF_MIN: u64 = 32 * 1024 * 1024;

struct StreamState {
    file_id: usize,
    current_piece: ValidPieceIndex,
    file_len: u64,
    waker: Option<Waker>,
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

    // Queue 1st, 2nd etc pieces from each stream in turn until they get 1/10th of the file .
    pub(crate) fn iter_next_pieces(
        &self,
        lengths: &Lengths,
    ) -> impl Iterator<Item = ValidPieceIndex> {
        let all = self
            .streams
            .iter()
            .map(|s| {
                let remaining = (s.value().file_len
                    + lengths.piece_length(s.value().current_piece) as u64)
                    .div_ceil(PER_STREAM_BUF_PART)
                    .max(PER_STREAM_BUF_MIN);
                (s.value().current_piece, remaining)
            })
            .map(Some)
            .collect_vec();

        struct It {
            all: Vec<Option<(ValidPieceIndex, u64)>>,
            lengths: Lengths,
        }

        impl Iterator for It {
            type Item = ValidPieceIndex;

            fn next(&mut self) -> Option<Self::Item> {
                for item in self.all.iter_mut() {
                    if let Some((p, remaining)) = item {
                        let y = *p;
                        let pl = self.lengths.piece_length(y);
                        *remaining = remaining.saturating_sub(pl as u64);
                        if *remaining == 0 {
                            *item = None;
                        } else if let Some(next_p) = self.lengths.validate_piece_index(y.get() + 1)
                        {
                            *item = Some((next_p, *remaining))
                        } else {
                            *item = None;
                        }
                        return Some(y);
                    }
                }
                None
            }
        }
        It {
            all,
            lengths: *lengths,
        }
    }

    pub(crate) fn wake_streams_on_piece_completed(&self, piece_id: ValidPieceIndex) {
        for mut w in self.streams.iter_mut() {
            if w.value().current_piece == piece_id {
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

    fn drop_stream(&self, stream_id: StreamId) {
        trace!(stream_id, "dropping stream");
        self.streams.remove(&stream_id);
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

        // determine the piece that position is pointing to.
        let lengths = &self.torrent.info().lengths;
        let dpl = lengths.default_piece_length();

        let abs_pos = self.file_torrent_abs_offset + self.position;
        let piece_id = abs_pos / dpl as u64;
        let piece_id: u32 = poll_try_io!(piece_id.try_into());

        let piece_id = poll_try_io!(lengths
            .validate_piece_index(piece_id)
            .context("bug: invalid piece"));
        let piece_len = lengths.piece_length(piece_id);
        let piece_offset = abs_pos % dpl as u64;
        let piece_remaining = piece_len as u64 - piece_offset;

        // queue N pieces after this if not yet. The "if let" should never fail.
        if let Some(mut s) = self.streams.streams.get_mut(&self.stream_id) {
            s.value_mut().current_piece = piece_id;
        }

        // if the piece is not there, register to wake when it is
        // check if we have the piece for real
        let have = poll_try_io!(self.torrent.with_chunk_tracker(|ct| {
            let have = ct.get_have_pieces()[piece_id.get() as usize];
            if !have {
                self.streams
                    .register_waker(self.stream_id, cx.waker().clone());
            }
            have
        }));
        if !have {
            trace!(stream_id = self.stream_id, file_id = self.file_id, piece_id = %piece_id, "poll pending, not have");
            return Poll::Pending;
        }

        // actually stream the piece
        let buf = tbuf.initialize_unfilled();
        let file_remaining = self.file_len - self.position;
        let bytes_to_read: usize = poll_try_io!((piece_len as u64)
            .min(buf.len() as u64)
            .min(piece_remaining)
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
        self.streams.drop_stream(self.stream_id)
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

    fn maybe_reconnect_needed_peers_for_file(&self, file_id: usize) {
        // If we have the full file, don't bother.
        if let Ok(true) = self.with_opened_file(file_id, |f| f.approx_is_finished()) {
            return;
        }
        self.with_state(|state| {
            if let crate::ManagedTorrentState::Live(l) = &state {
                l.reconnect_all_not_needed_peers();
            }
        })
    }

    pub fn stream(self: Arc<Self>, file_id: usize) -> anyhow::Result<FileStream> {
        let (fd_len, fd_offset) =
            self.with_opened_file(file_id, |fd| (fd.len, fd.offset_in_torrent))?;
        let streams = self.streams()?;
        let first_piece = self.info().lengths.validate_piece_index(0).context("bug")?;
        let s = FileStream {
            stream_id: streams.next_id(),
            streams: streams.clone(),
            file_id,
            position: 0,

            file_len: fd_len,
            file_torrent_abs_offset: fd_offset,
            torrent: self,
        };
        s.torrent.maybe_reconnect_needed_peers_for_file(file_id);
        streams.streams.insert(
            s.stream_id,
            StreamState {
                file_id,
                current_piece: first_piece,
                waker: None,
                file_len: s.file_len,
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
