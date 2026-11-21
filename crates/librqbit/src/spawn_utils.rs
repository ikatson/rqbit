use anyhow::Context;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// A tool to limit the number of blocking threads used concurrently to prevent
/// runtime starvation.
///
/// Also shortcuts for single-threaded tokio runtime to simply call the function, unlike
/// "tokio::task::block_in_place" which would panic.
#[derive(Clone, Debug)]
pub struct BlockingSpawner {
    allow_block_in_place: bool,
    concurrent_block_in_place_semaphore: Arc<Semaphore>,
}

impl BlockingSpawner {
    pub fn new(max_blocking_threads: usize) -> Self {
        let handle = tokio::runtime::Handle::current();
        let allow_block_in_place = match handle.runtime_flavor() {
            tokio::runtime::RuntimeFlavor::CurrentThread => false,
            tokio::runtime::RuntimeFlavor::MultiThread => true,
            _ => true,
        };
        Self {
            allow_block_in_place,
            concurrent_block_in_place_semaphore: Arc::new(Semaphore::new(
                max_blocking_threads.max(1),
            )),
        }
    }

    /// Only call this if you can't call the async function block_in_place_with_semaphore
    /// E.g. if you you have non-send objects on the stack.
    pub fn block_in_place<F: FnOnce() -> R, R>(&self, f: F) -> R {
        if self.allow_block_in_place {
            return tokio::task::block_in_place(f);
        }

        f()
    }

    /// like "block_in_place" but limit concurrency.
    pub async fn block_in_place_with_semaphore<F: FnOnce() -> R, R>(&self, f: F) -> R {
        if self.allow_block_in_place {
            let _permit = self
                .concurrent_block_in_place_semaphore
                .acquire()
                .await
                .unwrap();
            return tokio::task::block_in_place(f);
        }

        tokio::task::coop::consume_budget().await;
        f()
    }

    /// Run a `'static` blocking closure on Tokio's blocking thread pool while respecting
    /// the concurrency semaphore. Unlike `block_in_place_with_semaphore`, this offloads work
    /// from single-threaded runtimes as well and avoids pinning async worker cores.
    pub async fn spawn_blocking<F, R>(&self, f: F) -> anyhow::Result<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let permit = self
            .concurrent_block_in_place_semaphore
            .clone()
            .acquire_owned()
            .await
            .context("blocking spawner semaphore closed")?;
        let span = tracing::Span::current();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            span.in_scope(f)
        })
        .await
        .context("spawn_blocking task failed")
    }

    pub fn semaphore(&self) -> Arc<Semaphore> {
        self.concurrent_block_in_place_semaphore.clone()
    }
}
