use std::time::Duration;

use crate::peer_connection::with_timeout;
use anyhow::Context;
use buffers::ByteBuf;
use peer_binary_protocol::{
    Handshake, MessageBorrowed, MessageDeserializeError, PIECE_MESSAGE_DEFAULT_LEN,
};
use tokio::io::AsyncReadExt;

const BUFLEN: usize = PIECE_MESSAGE_DEFAULT_LEN * 2;

pub struct ReadBuf {
    buf: [u8; BUFLEN],
    start: usize,
    len: usize,
}

macro_rules! advance {
    ($self:expr, $len:expr) => {
        $self.len -= $len;
        $self.start = ($self.start + $len) % BUFLEN;
    };
}

macro_rules! as_slice_ranges {
    ($self:expr) => {{
        let first_len = $self.len.min(crate::read_buf::BUFLEN - $self.start);
        let first = $self.start..$self.start + first_len;
        let second = 0..$self.len.saturating_sub(first_len);
        (first, second)
    }};
}

macro_rules! as_slices {
    ($self:expr) => {{
        let (first, second) = as_slice_ranges!($self);
        (&$self.buf[first], &$self.buf[second])
    }};
}

impl ReadBuf {
    pub fn new() -> Self {
        Self {
            buf: [0u8; BUFLEN],
            start: 0,
            len: 0,
        }
    }

    // Read the BT handshake.
    // This MUST be run as the first operation on the buffer.
    pub async fn read_handshake(
        &mut self,
        mut conn: impl AsyncReadExt + Unpin,
        timeout: Duration,
    ) -> anyhow::Result<Handshake<ByteBuf<'_>>> {
        self.len = with_timeout("reading", timeout, conn.read(&mut self.buf))
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

    fn make_contiguous(&mut self) {
        let mut new = [0u8; BUFLEN];
        let (first, second) = as_slices!(self);
        new[..first.len()].copy_from_slice(first);
        new[first.len()..first.len() + second.len()].copy_from_slice(second);
        self.buf = new;
        self.start = 0;
    }

    #[cfg(test)]
    fn advance(&mut self, len: usize) {
        advance!(self, len);
    }

    fn write_buf_for_read_range(&self) -> std::ops::Range<usize> {
        if self.len == BUFLEN {
            return 0..0;
        }
        let start = (self.start + self.len) % BUFLEN;
        // TODO: can this be written without if?
        let end = if start < self.start {
            self.start
        } else {
            BUFLEN
        };
        start..end
    }

    fn write_buf_for_read(&mut self) -> &mut [u8] {
        let range = self.write_buf_for_read_range();
        &mut self.buf[range]
    }

    // Read a message into the buffer, try to deserialize it and call the callback on it.
    pub async fn read_message(
        &mut self,
        mut conn: impl AsyncReadExt + Unpin,
        timeout: Duration,
    ) -> anyhow::Result<MessageBorrowed<'_>> {
        loop {
            let (first, second) = as_slices!(self);
            let mut need_additional_bytes = match MessageBorrowed::deserialize(first, second) {
                Err(MessageDeserializeError::NotEnoughData(d, ..)) => d,
                Err(MessageDeserializeError::NeedContiguous) => {
                    self.make_contiguous();
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
                let size = with_timeout("reading", timeout, conn.read(self.write_buf_for_read()))
                    .await
                    .context("error reading from peer")?;
                if size == 0 {
                    anyhow::bail!("disconnected while reading, read so far: {}", self.len)
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
    fn test_ringbuf_write_range() {
        let mut b = ReadBuf::new();
        assert_eq!(b.write_buf_for_read_range(), 0..BUFLEN);

        b.start = 10;
        b.len = 10;
        assert_eq!(b.write_buf_for_read_range(), 20..BUFLEN);

        b.start = BUFLEN - 100;
        b.len = 100;
        assert_eq!(b.write_buf_for_read_range(), 0..BUFLEN - 100);

        b.start = BUFLEN - 100;
        b.len = 120;
        assert_eq!(b.write_buf_for_read_range(), 20..BUFLEN - 100);

        b.start = BUFLEN - 100;
        b.len = BUFLEN - 1;
        assert_eq!(b.write_buf_for_read_range(), BUFLEN - 101..BUFLEN - 100);

        b.start = BUFLEN - 100;
        b.len = BUFLEN;
        assert_eq!(b.write_buf_for_read_range().len(), 0);
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
}
