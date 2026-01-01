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

        f()
    }

    pub fn semaphore(&self) -> Arc<Semaphore> {
        self.concurrent_block_in_place_semaphore.clone()
    }
}
