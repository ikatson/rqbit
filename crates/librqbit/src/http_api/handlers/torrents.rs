use std::{net::SocketAddr, str::FromStr};

use anyhow::Context;
use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
};
use bytes::Bytes;
use http::{
    HeaderMap, HeaderName, HeaderValue, StatusCode,
    header::{CONTENT_DISPOSITION, CONTENT_TYPE},
};
use librqbit_core::magnet::Magnet;
use serde::{Deserialize, Serialize};

use super::ApiState;
use crate::{
    AddTorrent, ApiError, CreateTorrentOptions, SUPPORTED_SCHEMES,
    api::{ApiTorrentListOpts, Result, TorrentIdOrHash},
    api_error::WithStatusError,
    http_api::timeout::Timeout,
    http_api_types::TorrentAddQueryParams,
    torrent_state::peer::stats::snapshot::{PeerStatsFilter, PeerStatsFilterState},
    type_aliases::BF,
};

pub async fn h_torrents_list(
    State(state): State<ApiState>,
    Query(opts): Query<ApiTorrentListOpts>,
) -> impl IntoResponse {
    axum::Json(state.api.api_torrent_list_ext(opts))
}

pub async fn h_torrents_post(
    State(state): State<ApiState>,
    Query(params): Query<TorrentAddQueryParams>,
    Timeout(timeout): Timeout<600_000, 3_600_000>,
    data: Bytes,
) -> Result<impl IntoResponse> {
    let is_url = params.is_url;
    let opts = params.into_add_torrent_options();
    let data = data.to_vec();
    let maybe_magnet = |data: &[u8]| -> bool {
        std::str::from_utf8(data)
            .ok()
            .and_then(|s| Magnet::parse(s).ok())
            .is_some()
    };
    let add = match is_url {
        Some(true) => AddTorrent::Url(
            String::from_utf8(data)
                .context("invalid utf-8 for passed URL")?
                .into(),
        ),
        Some(false) => AddTorrent::TorrentFileBytes(data.into()),

        // Guess the format.
        None if SUPPORTED_SCHEMES
            .iter()
            .any(|s| data.starts_with(s.as_bytes()))
            || maybe_magnet(&data) =>
        {
            AddTorrent::Url(
                String::from_utf8(data)
                    .context("invalid utf-8 for passed URL")?
                    .into(),
            )
        }
        _ => AddTorrent::TorrentFileBytes(data.into()),
    };
    tokio::time::timeout(timeout, state.api.api_add_torrent(add, Some(opts)))
        .await
        .context("timeout")?
        .map(axum::Json)
}

pub async fn h_torrent_details(
    State(state): State<ApiState>,
    Path(idx): Path<TorrentIdOrHash>,
) -> Result<impl IntoResponse> {
    state.api.api_torrent_details(idx).map(axum::Json)
}

pub async fn h_torrent_haves(
    State(state): State<ApiState>,
    Path(idx): Path<TorrentIdOrHash>,
    headers: HeaderMap,
) -> Result<impl IntoResponse> {
    fn generate_svg(bits: &BF, len: u32) -> String {
        if len == 0 {
            return r#"<svg width="100%" height="100" xmlns="http://www.w3.org/2000/svg"></svg>"#
                .to_string();
        }

        const HAVE_COLOR: &str = "#22c55e";
        const MISSING_COLOR: &str = "#374151";

        let bit_width = 100.0 / len as f64;
        let mut svg_segments = String::new();

        let mut bits_iter = bits.iter().map(|b| *b).enumerate().peekable();

        while let Some((i, value)) = bits_iter.next() {
            let mut count = 1;

            // Peek ahead to find how many subsequent bits have the same value
            while let Some((_, next_value)) = bits_iter.peek() {
                if *next_value == value {
                    count += 1;
                    bits_iter.next();
                } else {
                    break;
                }
            }

            let color = if value { HAVE_COLOR } else { MISSING_COLOR };
            let x_pos = i as f64 * bit_width;
            let segment_width = count as f64 * bit_width;

            svg_segments.push_str(&format!(
                r#"<rect x="{:.4}%" y="0" width="{:.4}%" height="100%" fill="{}" />"#,
                x_pos, segment_width, color
            ));
        }

        format!(
            r#"<svg width="100%" height="20" viewBox="0 0 100 100" preserveAspectRatio="none" xmlns="http://www.w3.org/2000/svg">
                {}
            </svg>"#,
            svg_segments
        )
    }

    let (bf, len) = state.api.api_dump_haves(idx)?;

    // Check if binary format is requested
    let wants_binary = headers
        .get(http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.contains("application/octet-stream"));

    if wants_binary {
        let bytes = bf.into_boxed_slice();
        Ok((
            [
                (
                    CONTENT_TYPE,
                    HeaderValue::from_static("application/octet-stream"),
                ),
                (
                    HeaderName::from_static("x-bitfield-len"),
                    HeaderValue::from_str(&len.to_string()).unwrap(),
                ),
            ],
            bytes,
        )
            .into_response())
    } else {
        let svg = generate_svg(&bf, len);
        Ok((
            [(CONTENT_TYPE, HeaderValue::from_static("image/svg+xml"))],
            svg,
        )
            .into_response())
    }
}

pub async fn h_torrent_stats_v0(
    State(state): State<ApiState>,
    Path(idx): Path<TorrentIdOrHash>,
) -> Result<impl IntoResponse> {
    state.api.api_stats_v0(idx).map(axum::Json)
}

pub async fn h_torrent_stats_v1(
    State(state): State<ApiState>,
    Path(idx): Path<TorrentIdOrHash>,
) -> Result<impl IntoResponse> {
    state.api.api_stats_v1(idx).map(axum::Json)
}

pub async fn h_peer_stats(
    State(state): State<ApiState>,
    Path(idx): Path<TorrentIdOrHash>,
    Query(filter): Query<PeerStatsFilter>,
) -> Result<impl IntoResponse> {
    state.api.api_peer_stats(idx, filter).map(axum::Json)
}

pub async fn h_torrent_action_pause(
    State(state): State<ApiState>,
    Path(idx): Path<TorrentIdOrHash>,
) -> Result<impl IntoResponse> {
    state
        .api
        .api_torrent_action_pause(idx)
        .await
        .map(axum::Json)
}

pub async fn h_torrent_action_start(
    State(state): State<ApiState>,
    Path(idx): Path<TorrentIdOrHash>,
) -> Result<impl IntoResponse> {
    state
        .api
        .api_torrent_action_start(idx)
        .await
        .map(axum::Json)
}

pub async fn h_torrent_action_forget(
    State(state): State<ApiState>,
    Path(idx): Path<TorrentIdOrHash>,
) -> Result<impl IntoResponse> {
    state
        .api
        .api_torrent_action_forget(idx)
        .await
        .map(axum::Json)
}

pub async fn h_torrent_action_delete(
    State(state): State<ApiState>,
    Path(idx): Path<TorrentIdOrHash>,
) -> Result<impl IntoResponse> {
    state
        .api
        .api_torrent_action_delete(idx)
        .await
        .map(axum::Json)
}

#[derive(Deserialize)]
pub struct UpdateOnlyFilesRequest {
    only_files: Vec<usize>,
}

pub async fn h_torrent_action_update_only_files(
    State(state): State<ApiState>,
    Path(idx): Path<TorrentIdOrHash>,
    axum::Json(req): axum::Json<UpdateOnlyFilesRequest>,
) -> Result<impl IntoResponse> {
    state
        .api
        .api_torrent_action_update_only_files(idx, &req.only_files.into_iter().collect())
        .await
        .map(axum::Json)
}

pub async fn h_session_stats(State(state): State<ApiState>) -> impl IntoResponse {
    axum::Json(state.api.api_session_stats())
}

pub async fn h_peer_stats_prometheus(
    State(state): State<ApiState>,
    Path(idx): Path<TorrentIdOrHash>,
) -> Result<impl IntoResponse> {
    let handle = state.api.mgr_handle(idx)?;

    let live = handle
        .live()
        .with_status_error(StatusCode::PRECONDITION_FAILED, "torrent is not live")?;

    let peer_stats = live.per_peer_stats_snapshot(PeerStatsFilter {
        state: PeerStatsFilterState::Live,
    });

    let mut buf = String::new();

    const NAME: &str = "rqbit_peer_fetched_bytes";

    use core::fmt::Write;
    writeln!(&mut buf, "# TYPE {NAME} counter").unwrap();
    for (addr, stats) in peer_stats.peers.iter() {
        // Filter out useless peers that never sent us much.
        const THRESHOLD: u64 = 1024 * 1024;
        if stats.counters.fetched_bytes >= THRESHOLD {
            writeln!(
                &mut buf,
                "{NAME}{{addr=\"{addr}\"}} {}",
                stats.counters.fetched_bytes - THRESHOLD
            )
            .unwrap();
        }
    }

    Ok(buf)
}

pub async fn h_metadata(
    State(state): State<ApiState>,
    Path(idx): Path<TorrentIdOrHash>,
) -> Result<impl IntoResponse> {
    let handle = state.api.mgr_handle(idx)?;

    let (filename, bytes) = handle
        .with_metadata(|meta| {
            (
                meta.info
                    .name_or_else(|| format!("torrent_{idx}"))
                    .into_owned(),
                meta.torrent_bytes.clone(),
            )
        })
        .map_err(ApiError::from)?;

    Ok((
        [(
            http::header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}.torrent\""),
        )],
        bytes,
    ))
}

#[derive(Serialize)]
struct AddPeersResult {
    added: usize,
}

pub async fn h_add_peers(
    State(state): State<ApiState>,
    Path(idx): Path<TorrentIdOrHash>,
    body: Bytes,
) -> Result<impl IntoResponse> {
    let handle = state.api.mgr_handle(idx)?;
    let live = handle.live().ok_or(crate::Error::TorrentIsNotLive)?;

    let body =
        std::str::from_utf8(&body).with_status_error(StatusCode::BAD_REQUEST, "invalid utf-8")?;

    let addrs = body
        .split('\n')
        .filter_map(|s| SocketAddr::from_str(s).ok());

    let mut count = 0;
    for addr in addrs {
        if live.add_peer_if_not_seen(addr)? {
            count += 1;
        }
    }

    Ok(axum::Json(AddPeersResult { added: count }))
}

#[derive(Default, Deserialize, Debug)]
enum CreateTorrentOutput {
    #[default]
    #[serde(rename = "magnet")]
    Magnet,
    #[serde(rename = "torrent")]
    Torrent,
}

#[derive(Default, Deserialize, Debug)]
pub struct HttpCreateTorrentOptions {
    #[serde(default)]
    output: CreateTorrentOutput,
    #[serde(default)]
    trackers: Vec<String>,
    name: Option<String>,
}

pub async fn h_create_torrent(
    State(state): State<ApiState>,
    axum_extra::extract::Query(opts): axum_extra::extract::Query<HttpCreateTorrentOptions>,
    body: Bytes,
) -> Result<impl IntoResponse> {
    if !state.opts.allow_create {
        return Err((
            StatusCode::FORBIDDEN,
            "creating torrents not allowed. Enable through CLI options",
        )
            .into());
    }

    let path = std::path::Path::new(
        std::str::from_utf8(body.as_ref())
            .with_status_error(StatusCode::BAD_REQUEST, "invalid utf-8")?,
    );

    let create_opts = CreateTorrentOptions {
        name: opts.name.as_deref(),
        trackers: opts.trackers,
        piece_length: None,
    };

    let (torrent, handle) = state
        .api
        .session()
        .create_and_serve_torrent(path, create_opts)
        .await?;

    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&handle.id().to_string()) {
        headers.insert("torrent-id", v);
    }
    if let Ok(v) = HeaderValue::from_str(&torrent.info_hash().as_string()) {
        headers.insert("torrent-info-hash", v);
    }

    match opts.output {
        CreateTorrentOutput::Magnet => {
            let magnet = torrent.as_magnet();
            Ok(magnet.to_string().into_response())
        }
        CreateTorrentOutput::Torrent => {
            let name = torrent
                .as_info()
                .info
                .data
                .name
                .as_ref()
                .map(|n| String::from_utf8_lossy(n.as_ref()))
                .unwrap_or("torrent".into());

            headers.insert(
                CONTENT_TYPE,
                HeaderValue::from_static("application/x-bittorrent"),
            );

            if let Ok(h) =
                HeaderValue::from_str(&format!("attachment; filename=\"{name}.torrent\""))
            {
                headers.insert(CONTENT_DISPOSITION, h);
            }

            Ok((headers, torrent.as_bytes()?).into_response())
        }
    }
}
