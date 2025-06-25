use std::fmt::Display;

use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, trace};

/// Spawns a future with tracing instrumentation.
#[track_caller]
pub fn spawn<E: Display + Send + 'static>(
    span: tracing::Span,
    fut: impl std::future::Future<Output = Result<(), E>> + Send + 'static,
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

#[track_caller]
pub fn spawn_with_cancel<E: Display + Send + 'static>(
    span: tracing::Span,
    cancellation_token: CancellationToken,
    fut: impl std::future::Future<Output = Result<(), E>> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    spawn(span, async move {
        tokio::select! {
            _ = cancellation_token.cancelled() => {
                debug!("task cancelled");
                Ok(())
            },
            r = fut => r
        }
    })
}
