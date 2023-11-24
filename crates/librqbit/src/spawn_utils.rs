use tracing::{debug, trace, Instrument};

pub fn spawn(
    span: tracing::Span,
    fut: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    let fut = async move {
        trace!("started");
        match fut.await {
            Ok(_) => {
                debug!("finished");
            }
            Err(e) => {
                debug!("finished with error: {:#}", e)
            }
        }
    }
    .instrument(span.or_current());
    tokio::spawn(fut)
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

impl Default for BlockingSpawner {
    fn default() -> Self {
        let allow_block_in_place = match tokio::runtime::Handle::current().runtime_flavor() {
            tokio::runtime::RuntimeFlavor::CurrentThread => false,
            tokio::runtime::RuntimeFlavor::MultiThread => true,
            _ => true,
        };
        Self::new(allow_block_in_place)
    }
}
