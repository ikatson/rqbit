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

macro_rules! as_slices {
    ($self:expr) => {{
        let first_len = $self.len.min(BUFLEN - $self.start);
        let first = &$self.buf[$self.start..$self.start + first_len];
        let second = &$self.buf[..$self.len.saturating_sub(first_len)];
        (first, second)
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
        todo!()
    }

    fn write_buf_for_read(&mut self) -> &mut [u8] {
        let start = (self.start + self.len) % BUFLEN;
        let end = if start < self.start {
            self.start
        } else {
            BUFLEN
        };
        &mut self.buf[start..end]
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
