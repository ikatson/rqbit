use axum::{extract::State, response::IntoResponse};
#[cfg(feature = "tracing-subscriber-utils")]
use futures::TryStreamExt;
#[cfg(feature = "tracing-subscriber-utils")]
use tracing::debug;

use super::ApiState;
use crate::api::Result;

pub async fn h_set_rust_log(
    State(state): State<ApiState>,
    new_value: String,
) -> Result<impl IntoResponse> {
    state.api.api_set_rust_log(new_value).map(axum::Json)
}

#[cfg(feature = "tracing-subscriber-utils")]
pub async fn h_stream_logs(State(state): State<ApiState>) -> Result<impl IntoResponse> {
    let s = state.api.api_log_lines_stream()?.map_err(|e| {
        debug!(error=%e, "stream_logs");
        e
    });
    Ok(axum::body::Body::from_stream(s))
}

#[cfg(not(feature = "tracing-subscriber-utils"))]
pub async fn h_stream_logs(_state: State<ApiState>) -> Result<impl IntoResponse> {
    Ok((
        http::StatusCode::NOT_IMPLEMENTED,
        "stream_logs requires tracing-subscriber-utils feature",
    )
        .into_response())
}
