use tracing::{debug, error, trace, Instrument};

pub fn spawn(
    span: tracing::Span,
    fut: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    let fut = async move {
        trace!("started");
        match fut.await {
            Ok(_) => {
                debug!("finished");
            }
            Err(e) => {
                error!("finished with error: {:#}", e)
            }
        }
    }
    .instrument(span);
    tokio::task::spawn(fut)
}
