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

pub fn spawn_block_in_place<F: FnOnce() -> R, R>(f: F) -> R {
    // Have this wrapper so that it's easy to switch to just f() when
    // using tokio's single-threaded runtime. Single-threaded runtime is
    // easier to read with time profilers.
    tokio::task::block_in_place(f)
}
