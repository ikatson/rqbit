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

pub fn spawn_blocking<T: Send + Sync + 'static, N: Display + 'static + Send>(
    name: N,
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> tokio::task::JoinHandle<anyhow::Result<T>> {
    debug!("starting blocking task \"{}\"", name);
    tokio::task::spawn_blocking(move || match f() {
        Ok(v) => {
            debug!("blocking task \"{}\" finished", name);
            Ok(v)
        }
        Err(e) => {
            error!("error in blocking task \"{}\": {:#}", name, &e);
            Err(e)
        }
    })
}
