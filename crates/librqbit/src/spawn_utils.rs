use std::fmt::Display;

use log::{debug, error};

pub fn spawn<N: Display + 'static + Send>(
    name: N,
    fut: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
) {
    debug!("starting task \"{}\"", &name);
    tokio::spawn(async move {
        match fut.await {
            Ok(_) => {
                debug!("task \"{}\" finished", &name);
            }
            Err(e) => {
                error!("error in task \"{}\": {:#}", &name, e)
            }
        }
    });
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
