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
