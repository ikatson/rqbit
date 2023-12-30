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
    // How many bytes into the buffer we have read from the connection.
    // New reads should go past this.
    filled: usize,
    // How many bytes have we successfully deserialized.
    processed: usize,
}

impl ReadBuf {
    pub fn new() -> Self {
        Self {
            buf: vec![0; PIECE_MESSAGE_DEFAULT_LEN * 2],
            filled: 0,
            processed: 0,
        }
    }

    fn prepare_for_read(&mut self, need_additional_bytes: usize) {
        // Ensure the buffer starts from the to-be-deserialized message.
        if self.processed > 0 {
            if self.filled > self.processed {
                self.buf.copy_within(self.processed..self.filled, 0);
            }
            self.filled -= self.processed;
            self.processed = 0;
        }

        // Ensure we have enough capacity to deserialize the message.
        if self.buf.len() < self.filled + need_additional_bytes {
            self.buf.reserve(need_additional_bytes);
            self.buf.resize(self.buf.capacity(), 0);
        }
    }

    // Read the BT handshake.
    // This MUST be run as the first operation on the buffer.
    pub async fn read_handshake(
        &mut self,
        mut conn: impl AsyncReadExt + Unpin,
        timeout: Duration,
    ) -> anyhow::Result<Handshake<ByteBuf<'_>>> {
        self.filled = with_timeout(timeout, conn.read(&mut self.buf))
            .await
            .context("error reading handshake")?;
        if self.filled == 0 {
            anyhow::bail!("peer disconnected while reading handshake");
        }
        let (h, size) = Handshake::deserialize(&self.buf[..self.filled])
            .map_err(|e| anyhow::anyhow!("error deserializing handshake: {:?}", e))?;
        self.processed = size;
        Ok(h)
    }

    // Read a message into the buffer, try to deserialize it and call the callback on it.
    // We can't return the message because of a borrow checker issue.
    pub async fn read_message(
        &mut self,
        mut conn: impl AsyncReadExt + Unpin,
        timeout: Duration,
        on_message: impl for<'a> FnOnce(MessageBorrowed<'a>) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        loop {
            let need_additional_bytes =
                match MessageBorrowed::deserialize(&self.buf[self.processed..self.filled]) {
                    Err(MessageDeserializeError::NotEnoughData(d, _)) => d,
                    Ok((msg, size)) => {
                        self.processed += size;
                        // Rust's borrow checker can't do this early return. So we are using a callback instead.
                        // return Ok(msg);
                        on_message(msg)?;
                        return Ok(());
                    }
                    Err(e) => return Err(e.into()),
                };
            self.prepare_for_read(need_additional_bytes);
            let size = with_timeout(timeout, conn.read(&mut self.buf[self.filled..]))
                .await
                .context("error reading from peer")?;
            if size == 0 {
                anyhow::bail!("disconnected while reading, read so far: {}", self.filled)
            }
            self.filled += size;
        }
    }
}
