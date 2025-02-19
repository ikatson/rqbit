use std::net::SocketAddr;

use anyhow::Context;
use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
};
use bytes::Bytes;
use http::StatusCode;
use librqbit_core::magnet::Magnet;
use serde::{Deserialize, Serialize};

use super::ApiState;
use crate::{
    api::{ApiTorrentListOpts, Result, TorrentIdOrHash},
    http_api::timeout::Timeout,
    http_api_types::TorrentAddQueryParams,
    torrent_state::peer::stats::snapshot::PeerStatsFilter,
    AddTorrent, ApiError, SUPPORTED_SCHEMES,
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
) -> Result<impl IntoResponse> {
    state.api.api_dump_haves(idx)
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

pub async fn h_metadata(
    State(state): State<ApiState>,
    Path(idx): Path<TorrentIdOrHash>,
) -> Result<impl IntoResponse> {
    let handle = state.api.mgr_handle(idx)?;

    let (filename, bytes) = handle
        .with_metadata(|meta| {
            (
                meta.name.clone().unwrap_or_else(|| format!("{}", idx)),
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
    let live = handle
        .live()
        .context("torrent is not live")
        .map_err(ApiError::from)?;

    let addrs = body
        .split(|c| *c == b'\n')
        .map(|l| {
            std::str::from_utf8(l)
                .context("invalid UTF-8")
                .and_then(|l| l.parse().context("cant parse SocketAddr"))
        })
        .collect::<anyhow::Result<Vec<SocketAddr>>>()
        .context("invalid input")
        .map_err(|e| ApiError::new_from_anyhow(StatusCode::BAD_REQUEST, e))?;

    let mut count = 0;
    for addr in addrs {
        if live.add_peer_if_not_seen(addr).map_err(ApiError::from)? {
            count += 1;
        }
    }

    Ok(axum::Json(AddPeersResult { added: count }))
}
