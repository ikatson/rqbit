use tracing::{error, trace, Instrument};

/// Spawns a future with tracing instrumentation.
pub fn spawn(
    span: tracing::Span,
    fut: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    let fut = async move {
        trace!("started");
        tokio::pin!(fut);
        let mut trace_interval = tokio::time::interval(std::time::Duration::from_secs(5));

        loop {
            tokio::select! {
                _ = trace_interval.tick() => {
                    trace!("still running");
                },
                r = &mut fut => {
                    match r {
                        Ok(_) => {
                            trace!("finished");
                        }
                        Err(e) => {
                            error!("finished with error: {:#}", e)
                        }
                    }
                    return;
                }
            }
        }
    }
    .instrument(span);
    tokio::task::spawn(fut)
}
