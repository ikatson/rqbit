use anyhow::bail;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, trace};

#[derive(Debug)]
struct CancelledError {}
impl std::error::Error for CancelledError {}
impl std::fmt::Display for CancelledError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

/// Spawns a future with tracing instrumentation.
#[track_caller]
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
                            if e.is::<CancelledError>() {
                                debug!("task cancelled")
                            } else {
                                error!("finished with error: {:#}", e)
                            }

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

#[track_caller]
pub fn spawn_with_cancel(
    span: tracing::Span,
    cancellation_token: CancellationToken,
    fut: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    spawn(span, async move {
        tokio::select! {
            _ = cancellation_token.cancelled() => {
                bail!(CancelledError{})
            },
            r = fut => r
        }
    })
}
