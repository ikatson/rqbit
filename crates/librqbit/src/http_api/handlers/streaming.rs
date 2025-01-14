use std::{io::SeekFrom, sync::Arc};

use anyhow::Context;
use axum::{
    extract::{Path, State},
    response::IntoResponse,
};
use http::{HeaderMap, HeaderValue, StatusCode};
use serde::Deserialize;
use tokio::io::AsyncSeekExt;
use tracing::trace;

use super::ApiState;
use crate::api::{Result, TorrentIdOrHash};

#[derive(Deserialize)]
pub struct StreamPathParams {
    id: TorrentIdOrHash,
    file_id: usize,
    #[serde(rename = "filename")]
    _filename: Option<Arc<str>>,
}

pub async fn h_torrent_stream_file(
    State(state): State<ApiState>,
    Path(StreamPathParams { id, file_id, .. }): Path<StreamPathParams>,
    headers: http::HeaderMap,
) -> Result<impl IntoResponse> {
    let mut stream = state.api.api_stream(id, file_id)?;
    let mut status = StatusCode::OK;
    let mut output_headers = HeaderMap::new();
    output_headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));

    const DLNA_TRANSFER_MODE: &str = "transferMode.dlna.org";
    const DLNA_GET_CONTENT_FEATURES: &str = "getcontentFeatures.dlna.org";
    const DLNA_CONTENT_FEATURES: &str = "contentFeatures.dlna.org";

    if headers
        .get(DLNA_TRANSFER_MODE)
        .map(|v| matches!(v.as_bytes(), b"Streaming" | b"streaming"))
        .unwrap_or(false)
    {
        output_headers.insert(DLNA_TRANSFER_MODE, HeaderValue::from_static("Streaming"));
    }

    if headers
        .get(DLNA_GET_CONTENT_FEATURES)
        .map(|v| v.as_bytes() == b"1")
        .unwrap_or(false)
    {
        output_headers.insert(
            DLNA_CONTENT_FEATURES,
            HeaderValue::from_static("DLNA.ORG_OP=01"),
        );
    }

    if let Ok(mime) = state.api.torrent_file_mime_type(id, file_id) {
        output_headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_str(mime).context("bug - invalid MIME")?,
        );
    }

    let range_header = headers.get(http::header::RANGE);
    trace!(torrent_id=%id, file_id=file_id, range=?range_header, "request for HTTP stream");

    if let Some(range) = range_header {
        let offset: Option<u64> = range
            .to_str()
            .ok()
            .and_then(|s| s.strip_prefix("bytes="))
            .and_then(|s| s.strip_suffix('-'))
            .and_then(|s| s.parse().ok());
        if let Some(offset) = offset {
            status = StatusCode::PARTIAL_CONTENT;
            stream
                .seek(SeekFrom::Start(offset))
                .await
                .context("error seeking")?;

            output_headers.insert(
                http::header::CONTENT_LENGTH,
                HeaderValue::from_str(&format!("{}", stream.len() - stream.position()))
                    .context("bug")?,
            );
            output_headers.insert(
                http::header::CONTENT_RANGE,
                HeaderValue::from_str(&format!(
                    "bytes {}-{}/{}",
                    stream.position(),
                    stream.len().saturating_sub(1),
                    stream.len()
                ))
                .context("bug")?,
            );
        }
    } else {
        output_headers.insert(
            http::header::CONTENT_LENGTH,
            HeaderValue::from_str(&format!("{}", stream.len())).context("bug")?,
        );
    }

    let s = tokio_util::io::ReaderStream::with_capacity(stream, 65536);
    Ok((status, (output_headers, axum::body::Body::from_stream(s))))
}
