use std::{process::Stdio, sync::Arc};

use anyhow::Context;
use axum::{
    extract::{Path, State},
    response::IntoResponse,
};
use http::HeaderMap;
use serde::Deserialize;
use tracing::debug;

use super::ApiState;
use crate::api::{Result, TorrentIdOrHash};

#[derive(Deserialize)]
pub struct TranscodePathParams {
    id: TorrentIdOrHash,
    file_id: usize,
    #[serde(rename = "filename")]
    filename: Option<Arc<str>>,
}

/// Probe the stream URL for DTS audio streams and return their per-codec audio stream indices.
async fn probe_dts_audio_indices(url: &str) -> Vec<usize> {
    let result = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_streams",
            url,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await;

    let output = match result {
        Ok(o) => o,
        Err(e) => {
            debug!(error=?e, "ffprobe not available, assuming no DTS streams");
            return vec![];
        }
    };

    let json: serde_json::Value = match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let streams = match json["streams"].as_array() {
        Some(s) => s,
        None => return vec![],
    };

    let mut audio_idx: usize = 0;
    let mut dts_indices = Vec::new();

    for stream in streams {
        if stream["codec_type"].as_str() == Some("audio") {
            if stream["codec_name"].as_str() == Some("dts") {
                dts_indices.push(audio_idx);
            }
            audio_idx += 1;
        }
    }

    dts_indices
}

pub async fn h_torrent_transcode_file(
    State(state): State<ApiState>,
    Path(TranscodePathParams { id, file_id, filename }): Path<TranscodePathParams>,
    headers: HeaderMap,
) -> Result<impl IntoResponse> {
    // Extract port from Host header so ffmpeg can reach the stream endpoint on localhost.
    let host = headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost:3030");
    let port = host.split(':').nth(1).unwrap_or("3030");

    let url_path = filename
        .as_deref()
        .unwrap_or("")
        .to_owned();

    let stream_url = if url_path.is_empty() {
        format!("http://127.0.0.1:{port}/torrents/{id}/stream/{file_id}")
    } else {
        format!("http://127.0.0.1:{port}/torrents/{id}/stream/{file_id}/{url_path}")
    };

    debug!(stream_url, "probing DTS streams for transcode");
    let dts_indices = probe_dts_audio_indices(&stream_url).await;
    debug!(?dts_indices, "DTS audio stream indices");

    // Build ffmpeg args: video passthrough, audio passthrough, DTS streams → AC3.
    let mut args: Vec<String> = vec![
        "-v".into(),
        "quiet".into(),
        "-i".into(),
        stream_url,
        "-map".into(),
        "0".into(),
        "-c:v".into(),
        "copy".into(),
        "-c:a".into(),
        "copy".into(),
    ];

    for idx in &dts_indices {
        args.push(format!("-c:a:{idx}"));
        args.push("ac3".into());
    }

    args.push("-f".into());
    args.push("mpegts".into());
    args.push("pipe:1".into());

    let mut child = tokio::process::Command::new("ffmpeg")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn ffmpeg — is it installed?")?;

    let stdout = child.stdout.take().expect("stdout is piped");

    // Wait on the child in the background to avoid zombie processes.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });

    let stream = tokio_util::io::ReaderStream::with_capacity(stdout, 65536);
    Ok((
        [(http::header::CONTENT_TYPE, "video/mp2t")],
        axum::body::Body::from_stream(stream),
    ))
}
