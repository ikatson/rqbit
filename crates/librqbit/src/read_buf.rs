use std::{io::IoSliceMut, time::Duration};

use crate::{
    peer_connection::with_timeout, type_aliases::BoxAsyncRead,
    vectored_traits::AsyncReadVectoredExt,
};
use anyhow::{Context, bail};
use peer_binary_protocol::{
    Handshake, MAX_MSG_LEN, Message, MessageBorrowed, MessageDeserializeError,
};
use tokio::io::AsyncReadExt;

// We could work with just MAX_MSG_LEN buffer, but have it a bit bigger to reduce read() calls.
// TODO: consider setting it though to just MAX_MSG_LEN
const BUFLEN: usize = MAX_MSG_LEN * 2;

/// A ringbuffer for reading bittorrent messages from socket.
/// Messages may thus span 2 slices (notably, Piece message), which is reflected in their contents.
pub struct ReadBuf {
    buf: Box<[u8; BUFLEN]>,
    start: usize,
    len: usize,
}

/// Advance by N bytes
macro_rules! advance {
    ($self:expr, $len:expr) => {
        $self.len -= $len;
        $self.start = ($self.start + $len) % BUFLEN;
    };
}

/// Convert into 2 slices (as ranges)
macro_rules! as_slice_ranges {
    ($self:expr) => {{
        let first_len = $self.len.min(crate::read_buf::BUFLEN - $self.start);
        let first = $self.start..$self.start + first_len;
        let second = 0..$self.len.saturating_sub(first_len);
        (first, second)
    }};
}

/// Convert into 2 slices
macro_rules! as_slices {
    ($self:expr) => {{
        let (first, second) = as_slice_ranges!($self);
        (&$self.buf[first], &$self.buf[second])
    }};
}

impl ReadBuf {
    pub fn new() -> Self {
        Self {
            buf: Box::new([0u8; BUFLEN]),
            start: 0,
            len: 0,
        }
    }

    // Read the BT handshake.
    // This MUST be run as the first operation on the buffer.
    pub async fn read_handshake(
        &mut self,
        conn: &mut BoxAsyncRead,
        timeout: Duration,
    ) -> anyhow::Result<Handshake> {
        self.len = with_timeout("reading", timeout, conn.read(&mut *self.buf))
            .await
            .context("error reading handshake")?;
        if self.len == 0 {
            anyhow::bail!("peer disconnected while reading handshake");
        }
        let (h, size) = Handshake::deserialize(&self.buf[..self.len])
            .context("error deserializing handshake")?;
        advance!(self, size);
        Ok(h)
    }

    fn is_contiguous(&self) -> bool {
        self.start + self.len == (self.start + self.len) % BUFLEN
    }

    // In rare cases, we might need to make the buffer contiguous, as the message
    // parsing code won't work with a split ringbuffer. Only "bitfield" and "extended" messages
    // need contiguous buffers.
    fn make_contiguous(&mut self) -> anyhow::Result<()> {
        if self.is_contiguous() {
            bail!(
                "bug: make_contiguous() called on a contiguous buffer; start={} len={}",
                self.start,
                self.len
            );
        }

        // This should be so rare that it's not worth writing complex in-place code.
        let mut new = [0u8; BUFLEN];
        let (first, second) = as_slices!(self);
        new[..first.len()].copy_from_slice(first);
        new[first.len()..first.len() + second.len()].copy_from_slice(second);
        *self.buf = new;
        self.start = 0;
        Ok(())
    }

    #[cfg(test)]
    fn advance(&mut self, len: usize) {
        advance!(self, len);
    }

    fn is_full(&self) -> bool {
        self.len == BUFLEN
    }

    /// Get a part of the buffer for reading into.
    fn unfilled_ioslices(&mut self) -> [IoSliceMut; 2] {
        if self.is_full() {
            return [IoSliceMut::new(&mut []), IoSliceMut::new(&mut [])];
        }

        // Wraparound case. TODO: figure out if it's >=
        if self.start + self.len >= BUFLEN {
            // [..len..first..start..]
            [
                IoSliceMut::new(&mut self.buf[(self.start + self.len) % BUFLEN..self.start]),
                IoSliceMut::new(&mut []),
            ]
        } else {
            // [second...start..len..first]
            let (second, first) = self.buf.split_at_mut(self.start + self.len);
            let second = &mut second[..self.start];
            [IoSliceMut::new(first), IoSliceMut::new(second)]
        }
    }

    /// Read a message into the buffer, try to deserialize it and call the callback on it.
    pub async fn read_message(
        &mut self,
        mut conn: impl AsyncReadVectoredExt + Unpin,
        timeout: Duration,
    ) -> anyhow::Result<MessageBorrowed<'_>> {
        loop {
            let (first, second) = as_slices!(self);
            let (mut need_additional_bytes, ne) = match Message::deserialize(first, second) {
                Err(ne @ MessageDeserializeError::NotEnoughData(d, ..)) => (d, ne),
                Err(MessageDeserializeError::NeedContiguous) => {
                    self.make_contiguous()?;
                    continue;
                }
                Ok((msg, size)) => {
                    advance!(self, size);

                    // Rust's borrow checker can't do this early return so resort to unsafe.
                    // This erases the lifetime so that it's happy.
                    let msg: MessageBorrowed<'_> =
                        unsafe { std::mem::transmute(msg as MessageBorrowed<'_>) };
                    return Ok(msg);
                }
                Err(e) => return Err(e.into()),
            };
            while need_additional_bytes > 0 {
                if self.is_full() {
                    anyhow::bail!(
                        "read buffer is full. need_additional_bytes={need_additional_bytes}, last_err={ne:?}"
                    );
                }
                let size = with_timeout(
                    "reading",
                    timeout,
                    conn.read_vectored(&mut self.unfilled_ioslices()),
                )
                .await
                .context("error reading from peer")?;
                if size == 0 {
                    anyhow::bail!("peer disconected")
                }
                self.len += size;
                need_additional_bytes = need_additional_bytes.saturating_sub(size)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BUFLEN, ReadBuf};

    #[test]
    fn test_ringbuf_ranges() {
        let mut b = ReadBuf::new();
        assert_eq!(as_slice_ranges!(b), (0..0, 0..0));

        b.start = 10;
        b.len = 10;
        assert_eq!(as_slice_ranges!(b), (10..20, 0..0));

        b.start = BUFLEN - 100;
        b.len = 100;
        assert_eq!(as_slice_ranges!(b), (BUFLEN - 100..BUFLEN, 0..0));

        b.start = BUFLEN - 100;
        b.len = 120;
        assert_eq!(as_slice_ranges!(b), (BUFLEN - 100..BUFLEN, 0..20));

        b.start = BUFLEN - 100;
        b.len = BUFLEN;
        assert_eq!(as_slice_ranges!(b), (BUFLEN - 100..BUFLEN, 0..BUFLEN - 100));
    }

    #[test]
    fn test_ringbuf_advance() {
        let mut b = ReadBuf::new();

        b.start = 10;
        b.len = 10;
        assert_eq!(as_slice_ranges!(b), (10..20, 0..0));
        b.advance(5);
        assert_eq!(as_slice_ranges!(b), (15..20, 0..0));

        b.start = BUFLEN - 5;
        b.len = 10;
        assert_eq!(as_slice_ranges!(b), (BUFLEN - 5..BUFLEN, 0..5));
        b.advance(5);
        assert_eq!(as_slice_ranges!(b), (0..5, 0..0));
    }

    #[test]
    fn test_ringbuf_make_contiguous() {
        let mut b = ReadBuf::new();
        assert!(b.is_contiguous());
        assert!(b.make_contiguous().is_err());

        fn fill(buf: &mut [u8], value: u8) {
            for b in buf.iter_mut() {
                *b = value;
            }
        }

        fill(&mut b.buf[BUFLEN - 100..], 42);
        fill(&mut b.buf[..200], 43);
        b.start = BUFLEN - 100;
        b.len = 300;
        assert!(!b.is_contiguous());
        assert!(b.make_contiguous().is_ok());
        assert_eq!(b.len, 300);
        assert_eq!(b.start, 0);
        assert_eq!(&b.buf[..100], &[42u8; 100]);
        assert_eq!(&b.buf[100..300], &[43u8; 200]);
        assert_eq!(&b.buf[300..], &[0u8; BUFLEN - 300]);
    }

    #[test]
    fn test_unfilled_ioslices() {
        ReadBuf::new();

        fn offset(buf: &ReadBuf, s: *const u8, len: usize) -> std::ops::Range<usize> {
            if len == 0 {
                return 0..0;
            }
            let offset = unsafe { s.byte_offset_from(buf.buf.as_ptr()) } as usize;
            offset..offset + len
        }

        fn offsets(
            buf: &ReadBuf,
            ioslices: [(*const u8, usize); 2],
        ) -> [std::ops::Range<usize>; 2] {
            [
                offset(buf, ioslices[0].0, ioslices[0].1),
                offset(buf, ioslices[1].0, ioslices[1].1),
            ]
        }

        #[track_caller]
        fn assert_one(start: usize, len: usize, ranges: [std::ops::Range<usize>; 2]) {
            let mut b = ReadBuf::new();
            b.start = start;
            b.len = len;
            let [f, s] = b.unfilled_ioslices();
            let slices = [(f.as_ptr(), f.len()), (s.as_ptr(), s.len())];
            assert_eq!(offsets(&b, slices), ranges)
        }

        // full
        assert_one(0, BUFLEN, [0..0, 0..0]);
        assert_one(100, BUFLEN, [0..0, 0..0]);
        assert_one(BUFLEN, BUFLEN, [0..0, 0..0]);

        // start=0
        assert_one(0, 100, [100..BUFLEN, 0..0]);
        assert_one(0, BUFLEN - 100, [BUFLEN - 100..BUFLEN, 0..0]);
        assert_one(0, BUFLEN - 1, [BUFLEN - 1..BUFLEN, 0..0]);

        // start=N
        assert_one(100, 100, [200..BUFLEN, 0..100]);
        assert_one(100, BUFLEN - 100, [0..100, 0..0]);
        assert_one(100, BUFLEN - 101, [BUFLEN - 1..BUFLEN, 0..100]);

        // start=BUFLEN-1
        assert_one(BUFLEN - 1, 100, [99..BUFLEN - 1, 0..0]);
    }
}
