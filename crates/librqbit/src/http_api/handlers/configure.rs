use axum::{
    Json,
    extract::{Path, State},
    response::IntoResponse,
};

use super::ApiState;
use crate::{
    api::{EmptyJsonResponse, Result, TorrentIdOrHash},
    limits::LimitsConfig,
};

pub async fn h_update_session_ratelimits(
    State(state): State<ApiState>,
    Json(limits): Json<LimitsConfig>,
) -> Result<impl IntoResponse> {
    state
        .api
        .session()
        .ratelimits
        .set_upload_bps(limits.upload_bps);
    state
        .api
        .session()
        .ratelimits
        .set_download_bps(limits.download_bps);
    Ok(Json(EmptyJsonResponse {}))
}

pub async fn h_get_session_ratelimits(State(state): State<ApiState>) -> Result<impl IntoResponse> {
    let config = state.api.session().ratelimits.get_config();
    Ok(Json(config))
}

pub async fn h_update_torrent_ratelimits(
    State(state): State<ApiState>,
    Path(idx): Path<TorrentIdOrHash>,
    Json(limits): Json<LimitsConfig>,
) -> Result<impl IntoResponse> {
    let handle = state.api.mgr_handle(idx)?;
    state
        .api
        .session()
        .set_torrent_ratelimits(&handle, limits)
        .await?;
    Ok(Json(EmptyJsonResponse {}))
}

pub async fn h_get_torrent_ratelimits(
    State(state): State<ApiState>,
    Path(idx): Path<TorrentIdOrHash>,
) -> Result<impl IntoResponse> {
    Ok(Json(state.api.mgr_handle(idx)?.ratelimits()))
}
