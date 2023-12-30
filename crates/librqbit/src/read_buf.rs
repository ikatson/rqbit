use std::time::Duration;

use crate::peer_connection::with_timeout;
use anyhow::Context;
use buffers::ByteBuf;
use peer_binary_protocol::{
    Handshake, MessageBorrowed, MessageDeserializeError, PIECE_MESSAGE_DEFAULT_LEN,
};
use tokio::io::AsyncReadExt;

pub struct ReadBuf {
    buf: Vec<u8>,
    read_so_far: usize,
    last_size: usize,
}

impl ReadBuf {
    pub fn new() -> Self {
        Self {
            buf: vec![0; PIECE_MESSAGE_DEFAULT_LEN * 2],
            read_so_far: 0,
            last_size: 0,
        }
    }

    fn prepare_for_read(&mut self) {
        if self.read_so_far > self.last_size {
            self.buf.copy_within(self.last_size..self.read_so_far, 0);
        }
        self.read_so_far -= self.last_size;
        self.last_size = 0;
    }

    // This MUST be run as the first operation on the buffer.
    pub async fn read_handshake(
        &mut self,
        mut conn: impl AsyncReadExt + Unpin,
        timeout: Duration,
    ) -> anyhow::Result<Handshake<ByteBuf<'_>>> {
        self.read_so_far = with_timeout(timeout, conn.read(&mut self.buf))
            .await
            .context("error reading handshake")?;
        if self.read_so_far == 0 {
            anyhow::bail!("bad handshake");
        }
        let (h, size) = Handshake::deserialize(&self.buf[..self.read_so_far])
            .map_err(|e| anyhow::anyhow!("error deserializing handshake: {:?}", e))?;
        self.last_size = size;
        Ok(h)
    }

    pub async fn read_message(
        &mut self,
        mut conn: impl AsyncReadExt + Unpin,
        timeout: Duration,
        on_message: impl for<'a> FnOnce(MessageBorrowed<'a>) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.prepare_for_read();
        loop {
            let need_additional_bytes =
                match MessageBorrowed::deserialize(&self.buf[..self.read_so_far]) {
                    Err(MessageDeserializeError::NotEnoughData(d, _)) => d,
                    Ok((msg, size)) => {
                        self.last_size = size;
                        // Rust's borrow checker can't do this early return. So we are using a callback instead.
                        // return Ok(msg);
                        on_message(msg)?;
                        return Ok(());
                    }
                    Err(e) => return Err(e.into()),
                };
            if self.buf.len() < self.read_so_far + need_additional_bytes {
                self.buf.reserve(need_additional_bytes);
                self.buf.resize(self.buf.capacity(), 0);
            }
            let size = with_timeout(timeout, conn.read(&mut self.buf[self.read_so_far..]))
                .await
                .context("error reading from peer")?;
            if size == 0 {
                anyhow::bail!(
                    "disconnected while reading, read so far: {}",
                    self.read_so_far
                )
            }
            self.read_so_far += size;
        }
    }
}
