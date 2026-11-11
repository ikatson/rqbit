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
use serde::{Deserialize, Serialize};

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

/// The body of the super seeding endpoints. The key is required.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuperSeeding {
    pub super_seeding: bool,
}

pub async fn h_update_torrent_super_seeding(
    State(state): State<ApiState>,
    Path(idx): Path<TorrentIdOrHash>,
    Json(req): Json<SuperSeeding>,
) -> Result<impl IntoResponse> {
    let handle = state.api.mgr_handle(idx)?;
    state
        .api
        .session()
        .set_super_seeding(&handle, req.super_seeding)
        .await?;
    Ok(Json(EmptyJsonResponse {}))
}

pub async fn h_get_torrent_super_seeding(
    State(state): State<ApiState>,
    Path(idx): Path<TorrentIdOrHash>,
) -> Result<impl IntoResponse> {
    Ok(Json(SuperSeeding {
        super_seeding: state.api.mgr_handle(idx)?.super_seeding(),
    }))
}

#[cfg(test)]
mod tests {
    use super::SuperSeeding;

    #[test]
    fn test_super_seeding_body() {
        let parse = |body: &str| {
            serde_json::from_str::<SuperSeeding>(body)
                .ok()
                .map(|s| s.super_seeding)
        };
        assert_eq!(parse(r#"{"super_seeding": true}"#), Some(true));
        assert_eq!(parse(r#"{"super_seeding": false}"#), Some(false));
        assert_eq!(parse(r#"{}"#), None);
        assert_eq!(parse(r#"{"super_seeding": 1}"#), None);
        assert_eq!(parse(r#"{"superseeding": true}"#), None);
    }
}
