//! Transparent RC4 stream wrappers for post-handshake traffic.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::rc4::Rc4;

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
}

impl<W> Rc4Writer<W> {
    pub fn new(inner: W, rc4: Rc4) -> Self {
        Self { inner, rc4 }
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

        // Encrypt with a cloned state. Pending and errors leave the formal RC4
        // state untouched; a short write commits exactly the accepted prefix.
        let mut trial = this.rc4.clone();
        let mut encrypted = data.to_vec();
        trial.apply_keystream(&mut encrypted);
        match Pin::new(&mut this.inner).poll_write(cx, &encrypted) {
            Poll::Ready(Ok(n)) => {
                let accepted = n.min(data.len());
                this.rc4.apply_keystream(&mut encrypted[..accepted]);
                Poll::Ready(Ok(accepted))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
                Some(Action::Error) => Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::Other,
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
        Rc4::new(key).apply_keystream(&mut bytes);
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
        let mut writer = Rc4Writer::new(inner, Rc4::new(key));
        writer.write_all(b"abcdefgh").await?;
        let actual = sink
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "poisoned test lock"))?
            .clone();
        assert_eq!(actual, expected(key, b"abcdefgh"));
        Ok(())
    }

    #[tokio::test]
    async fn write_error_does_not_advance_state() -> io::Result<()> {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let inner = FaultWriter {
            actions: VecDeque::from([Action::Error]),
            bytes: sink.clone(),
        };
        let key = b"writer-error";
        let mut writer = Rc4Writer::new(inner, Rc4::new(key));
        assert!(writer.write(b"discarded").await.is_err());
        writer.write_all(b"accepted").await?;
        let actual = sink
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "poisoned test lock"))?
            .clone();
        assert_eq!(actual, expected(key, b"accepted"));
        Ok(())
    }
}
