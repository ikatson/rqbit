use std::{future::poll_fn, io::IoSliceMut, pin::Pin, task::Poll};

use tokio::io::AsyncRead;

pub trait AsyncReadVectored: AsyncRead + Unpin {
    fn poll_read_vectored(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        vec: &mut [IoSliceMut<'_>],
    ) -> Poll<std::io::Result<usize>>;
}

impl<T: ?Sized + AsyncReadVectored> AsyncReadVectored for &mut T {
    fn poll_read_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        vec: &mut [IoSliceMut<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut **self).poll_read_vectored(cx, vec)
    }
}

impl<T: ?Sized + AsyncReadVectored> AsyncReadVectored for Box<T> {
    fn poll_read_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        vec: &mut [IoSliceMut<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut **self).poll_read_vectored(cx, vec)
    }
}

pub trait AsyncReadVectoredExt {
    async fn read_vectored(&mut self, vec: &mut [IoSliceMut<'_>]) -> std::io::Result<usize>;
}

impl<T: AsyncReadVectored> AsyncReadVectoredExt for T {
    async fn read_vectored(&mut self, vec: &mut [IoSliceMut<'_>]) -> std::io::Result<usize> {
        poll_fn(|cx| Pin::new(&mut *self).poll_read_vectored(cx, vec)).await
    }
}

impl AsyncReadVectored for tokio::net::tcp::OwnedReadHalf {
    fn poll_read_vectored(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        vec: &mut [IoSliceMut<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        loop {
            match this.try_read_vectored(vec) {
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::task::ready!(this.as_ref().poll_read_ready(cx)?);
                    continue;
                }
                res => return Poll::Ready(res),
            }
        }
    }
}

pub struct AsyncReadToVectoredCompat<T>(T);

impl<T: AsyncRead + Unpin> AsyncRead for AsyncReadToVectoredCompat<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl<T: AsyncRead + Unpin> AsyncReadVectored for AsyncReadToVectoredCompat<T> {
    // async fn read_vectored(&mut self, vec: &mut [IoSliceMut<'_>]) -> std::io::Result<usize> {
    //     let first_non_empty = match vec.iter_mut().find(|s| !s.is_empty()) {
    //         Some(s) => &mut **s,
    //         None => return Ok(0),
    //     };
    //     self.read(first_non_empty).await
    // }

    fn poll_read_vectored(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        vec: &mut [IoSliceMut<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let first_non_empty = match vec.iter_mut().find(|s| !s.is_empty()) {
            Some(s) => &mut **s,
            None => return Poll::Ready(Ok(0)),
        };
        let mut rbuf = tokio::io::ReadBuf::new(first_non_empty);
        std::task::ready!(self.poll_read(cx, &mut rbuf)?);
        Poll::Ready(Ok(rbuf.filled().len()))
    }
}

pub trait AsyncReadVectoredIntoCompat: Sized {
    fn into_vectored_compat(self) -> AsyncReadToVectoredCompat<Self>;
}

impl<T: AsyncRead + Unpin + Sized> AsyncReadVectoredIntoCompat for T {
    fn into_vectored_compat(self) -> AsyncReadToVectoredCompat<Self> {
        AsyncReadToVectoredCompat(self)
    }
}
