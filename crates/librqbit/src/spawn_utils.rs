use tracing::{debug, error, trace, Instrument};

pub fn spawn(
    span: tracing::Span,
    fut: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
) {
    let fut = async move {
        trace!("started");
        match fut.await {
            Ok(_) => {
                debug!("finished");
            }
            Err(e) => {
                error!("{:#}", e)
            }
        }
    }
    .instrument(span.or_current());
    tokio::spawn(fut);
}

#[derive(Clone, Copy, Debug)]
pub struct BlockingSpawner {
    allow_tokio_block_in_place: bool,
}

impl BlockingSpawner {
    pub fn new(allow_tokio_block_in_place: bool) -> Self {
        Self {
            allow_tokio_block_in_place,
        }
    }
    pub fn spawn_block_in_place<F: FnOnce() -> R, R>(&self, f: F) -> R {
        if self.allow_tokio_block_in_place {
            return tokio::task::block_in_place(f);
        }

        f()
    }
}
