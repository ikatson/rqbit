/// Spawn a future inside a tracing span, while logging it's start,
/// finish and periodically logging if it's still alive.
pub fn spawn(
    _name: &str,
    span: tracing::Span,
    fut: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    librqbit_core::spawn_utils::spawn(span, fut)
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct BlockingSpawner {
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
