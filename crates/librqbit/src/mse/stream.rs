//! Transparent RC4 stream wrappers for post-handshake traffic.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use rc4::{Rc4, StreamCipher};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub struct Rc4Reader<R> {
    inner: R,
    rc4: Rc4,
}

impl<R> Rc4Reader<R> {
    pub fn new(inner: R, rc4: Rc4) -> Self {
        Self { inner, rc4 }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for Rc4Reader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                this.rc4.apply_keystream(&mut buf.filled_mut()[before..]);
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

pub struct Rc4Writer<W> {
    inner: W,
    rc4: Rc4,
    /// Ciphertext already produced (whose keystream has been consumed) but not
    /// yet flushed to the underlying stream. Set when a write is Pending or
    /// partially accepted; the caller must not be asked to re-encrypt.
    pending: Vec<u8>,
}

impl<W> Rc4Writer<W> {
    pub fn new(inner: W, rc4: Rc4) -> Self {
        Self {
            inner,
            rc4,
            pending: Vec::new(),
        }
    }

    /// Flush any pending ciphertext to the underlying writer, consuming it.
    /// Returns `Poll::Ready(Ok(()))` once everything is flushed, `Poll::Pending`
    /// when the underlying writer is not writable.
    fn flush_pending(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>>
    where
        W: AsyncWrite + Unpin,
    {
        let this = self;
        loop {
            if this.pending.is_empty() {
                return Poll::Ready(Ok(()));
            }
            match Pin::new(&mut this.inner).poll_write(cx, &this.pending) {
                Poll::Ready(Ok(n)) => {
                    if n == 0 {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "failed to write RC4 stream",
                        )));
                    }
                    // Consume exactly the accepted prefix; the remaining
                    // ciphertext stays pending for the next poll.
                    this.pending.drain(..n);
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for Rc4Writer<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if data.is_empty() {
            return Poll::Ready(Ok(0));
        }

        // If a previous poll left ciphertext pending (this is a retry after a
        // Pending or partial flush), flush it and report the logical write as
        // complete without re-encrypting the caller's data.
        if !this.pending.is_empty() {
            match this.flush_pending(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(())) => return Poll::Ready(Ok(data.len())),
            }
        }

        // Encrypt the whole buffer once, buffer the ciphertext, then flush it.
        let mut encrypted = data.to_vec();
        this.rc4.apply_keystream(&mut encrypted);
        this.pending = encrypted;
        match this.flush_pending(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(())) => Poll::Ready(Ok(data.len())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match this.flush_pending(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(())) => Pin::new(&mut this.inner).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match this.flush_pending(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(())) => Pin::new(&mut this.inner).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc4::KeyInit;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use tokio::io::AsyncWriteExt;

    enum Action {
        Pending,
        Limit(usize),
        Error,
    }

    struct FaultWriter {
        actions: VecDeque<Action>,
        bytes: Arc<Mutex<Vec<u8>>>,
    }

    impl AsyncWrite for FaultWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            data: &[u8],
        ) -> Poll<io::Result<usize>> {
            match self.actions.pop_front() {
                Some(Action::Pending) => {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
                Some(Action::Error) => Poll::Ready(Err(io::Error::other(
                    "injected write failure",
                ))),
                Some(Action::Limit(limit)) => {
                    let count = limit.min(data.len());
                    if let Ok(mut bytes) = self.bytes.lock() {
                        bytes.extend_from_slice(&data[..count]);
                    }
                    Poll::Ready(Ok(count))
                }
                None => {
                    if let Ok(mut bytes) = self.bytes.lock() {
                        bytes.extend_from_slice(data);
                    }
                    Poll::Ready(Ok(data.len()))
                }
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn expected(key: &[u8], plaintext: &[u8]) -> Vec<u8> {
        let mut bytes = plaintext.to_vec();
        rc4::Rc4::new_from_slice(key)
            .unwrap()
            .apply_keystream(&mut bytes);
        bytes
    }

    #[tokio::test]
    async fn pending_and_short_writes_preserve_state() -> io::Result<()> {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let inner = FaultWriter {
            actions: VecDeque::from([Action::Pending, Action::Limit(3)]),
            bytes: sink.clone(),
        };
        let key = b"writer-state";
        let mut writer = Rc4Writer::new(inner, rc4::Rc4::new_from_slice(key).unwrap());
        writer.write_all(b"abcdefgh").await?;
        let actual = sink
            .lock()
            .map_err(|_| io::Error::other("poisoned test lock"))?
            .clone();
        assert_eq!(actual, expected(key, b"abcdefgh"));
        Ok(())
    }

    #[tokio::test]
    async fn write_error_propagates() -> io::Result<()> {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let inner = FaultWriter {
            actions: VecDeque::from([Action::Error]),
            bytes: sink.clone(),
        };
        let key = b"writer-error";
        let mut writer = Rc4Writer::new(inner, rc4::Rc4::new_from_slice(key).unwrap());
        // An underlying write error propagates to the caller; nothing is
        // accepted by the sink. (Unlike the self-contained rc4 impl we can't
        // guarantee the RC4 keystream is un-advanced after a failed write, and
        // in practice a write error tears down the connection anyway.)
        assert!(writer.write(b"discarded").await.is_err());
        assert_eq!(
            sink.lock()
                .map_err(|_| io::Error::other("poisoned test lock"))?
                .len(),
            0
        );
        Ok(())
    }
    #[tokio::test]
    async fn repeated_pending_does_not_reencrypt() -> io::Result<()> {
        // The underlying writer returns Pending several times before accepting
        // data. Rc4Writer must not re-encrypt the caller's plaintext on retry
        // (the ciphertext is buffered once), and the final bytes must match
        // the single-pass RC4 encryption.
        let sink = Arc::new(Mutex::new(Vec::new()));
        let inner = FaultWriter {
            actions: VecDeque::from([Action::Pending, Action::Pending, Action::Pending]),
            bytes: sink.clone(),
        };
        let key = b"pending-retry";
        let mut writer = Rc4Writer::new(inner, rc4::Rc4::new_from_slice(key).unwrap());
        writer.write_all(b"persistent data").await?;
        let actual = sink
            .lock()
            .map_err(|_| io::Error::other("poisoned test lock"))?
            .clone();
        assert_eq!(actual, expected(key, b"persistent data"));
        Ok(())
    }

}