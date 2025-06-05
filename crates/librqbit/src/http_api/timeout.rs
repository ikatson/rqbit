use std::time::Duration;

use anyhow::Context;
use axum::{RequestPartsExt, extract::Query};
use http::request::Parts;
use serde::Deserialize;

use crate::ApiError;

pub struct Timeout<const DEFAULT_MS: usize, const MAX_MS: usize>(pub Duration);

impl<S, const DEFAULT_MS: usize, const MAX_MS: usize> axum::extract::FromRequestParts<S>
    for Timeout<DEFAULT_MS, MAX_MS>
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    /// Perform the extraction.
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        #[derive(Deserialize)]
        struct QueryT {
            timeout_ms: Option<usize>,
        }

        let q = parts
            .extract::<Query<QueryT>>()
            .await
            .context("error running Timeout extractor")?;

        let timeout_ms = q
            .timeout_ms
            .map(Ok)
            .or_else(|| {
                parts
                    .headers
                    .get("x-req-timeout-ms")
                    .map(|v| {
                        std::str::from_utf8(v.as_bytes()).context("invalid utf-8 in timeout value")
                    })
                    .map(|v| v.and_then(|v| v.parse::<usize>().context("invalid timeout integer")))
            })
            .transpose()
            .context("error parsing timeout")?
            .unwrap_or(DEFAULT_MS);
        let timeout_ms = timeout_ms.min(MAX_MS);
        Ok(Timeout(Duration::from_millis(timeout_ms as u64)))
    }
}
