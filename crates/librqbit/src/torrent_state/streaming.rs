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
use librqbit_core::lengths::ValidPieceIndex;
use tokio::io::{AsyncRead, AsyncSeek};

use crate::{opened_file::OpenedFile, ManagedTorrent};

use super::ManagedTorrentHandle;

type StreamId = usize;

#[derive(Default)]
pub(crate) struct TorrentStreams {
    next_stream_id: AtomicUsize,
    wakers_by_stream: DashMap<StreamId, (ValidPieceIndex, Waker)>,
}

impl TorrentStreams {
    fn next_id(&self) -> usize {
        self.next_stream_id.fetch_add(1, Ordering::Relaxed)
    }

    fn register_waker(&self, stream_id: StreamId, piece_id: ValidPieceIndex, waker: Waker) {
        self.wakers_by_stream.insert(stream_id, (piece_id, waker));
    }

    pub(crate) fn wake_streams_on_piece_completed(&self, piece_id: ValidPieceIndex) {
        let mut woken = Vec::new();
        for w in self.wakers_by_stream.iter() {
            if w.value().0 == piece_id {
                w.value().1.wake_by_ref();
                woken.push(*w.key());
            }
        }
        for w in woken {
            self.wakers_by_stream.remove(&w);
        }
    }

    fn drop_stream(&self, stream_id: StreamId) {
        self.wakers_by_stream.remove(&stream_id);
    }
}

struct FileStream {
    torrent: ManagedTorrentHandle,
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
            Err(e) => return Poll::Ready(Err(e)),
        }
    }};
}

impl AsyncRead for FileStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // if the file is over, return 0
        if self.position == self.file_len {
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

        // queue N pieces after this if not yet
        // TODO

        // if the piece is not there, register to wake when it is
        // check if we have the piece for real
        let have = poll_try_io!(self.torrent.with_chunk_tracker(|ct| {
            let have = ct.get_have_pieces()[piece_id.get() as usize];
            if !have {
                self.torrent
                    .streams
                    .register_waker(self.stream_id, piece_id, cx.waker().clone());
            }
            have
        }));
        if !have {
            return Poll::Pending;
        }

        // actually stream the piece
        let buf = buf.initialize_unfilled();
        let file_remaining = self.file_len - self.position;
        let bytes_to_read: usize = poll_try_io!((piece_len as u64)
            .min(buf.len() as u64)
            .min(piece_remaining)
            .min(file_remaining)
            .try_into());

        let buf = &mut buf[..bytes_to_read];

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
            SeekFrom::Start(s) => {
                self.as_mut().position = s;
                return Ok(());
            }
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
        self.torrent.streams.drop_stream(self.stream_id)
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

    pub fn stream(self: Arc<Self>, file_id: usize) -> anyhow::Result<impl AsyncRead + AsyncSeek> {
        let (fd_len, fd_offset) =
            self.with_opened_file(file_id, |fd| (fd.len, fd.offset_in_torrent))?;
        Ok(FileStream {
            stream_id: {
                let this = &self;
                &this.streams
            }
            .next_id(),

            file_id,
            position: 0,

            file_len: fd_len,
            file_torrent_abs_offset: fd_offset,
            torrent: self,
        })
    }
}
