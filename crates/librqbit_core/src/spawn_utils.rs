use anyhow::bail;
use tokio_util::sync::CancellationToken;
use tracing::{error, trace, Instrument};

/// Spawns a future with tracing instrumentation.
pub fn spawn<R: Send + 'static>(
    span: tracing::Span,
    fut: impl std::future::Future<Output = anyhow::Result<R>> + Send + 'static,
) -> tokio::task::JoinHandle<anyhow::Result<R>> {
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
                    match &r {
                        Ok(_) => {
                            trace!("finished");
                        }
                        Err(e) => {
                            error!("finished with error: {:#}", e)
                        }
                    }
                    return r;
                }
            }
        }
    }
    .instrument(span);
    tokio::task::spawn(fut)
}

pub fn spawn_with_cancel<R: Send + 'static>(
    span: tracing::Span,
    cancellation_token: CancellationToken,
    fut: impl std::future::Future<Output = anyhow::Result<R>> + Send + 'static,
) -> tokio::task::JoinHandle<anyhow::Result<R>> {
    spawn(span, async move {
        tokio::select! {
            _ = cancellation_token.cancelled() => {
                bail!("cancelled");
            },
            r = fut => r
        }
    })
}
