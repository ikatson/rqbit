use std::io::LineWriter;

use bytes::Bytes;
use tracing_subscriber::fmt::MakeWriter;

pub struct Subscriber {
    tx: tokio::sync::broadcast::Sender<Bytes>,
}

pub struct Writer {
    tx: tokio::sync::broadcast::Sender<Bytes>,
}

pub type LineRx = tokio::sync::broadcast::Receiver<Bytes>;

impl Subscriber {
    pub fn new() -> (Self, LineRx) {
        let (tx, rx) = tokio::sync::broadcast::channel(100);
        (Self { tx }, rx)
    }
}

impl<'a> MakeWriter<'a> for Subscriber {
    type Writer = LineWriter<Writer>;

    fn make_writer(&self) -> Self::Writer {
        LineWriter::new(Writer {
            tx: self.tx.clone(),
        })
    }
}

impl std::io::Write for Writer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let len = buf.len();
        let arc = buf.to_vec().into();
        let _ = self.tx.send(arc);
        Ok(len)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
