use axum::{extract::State, response::IntoResponse};

use super::ApiState;
use crate::api::Result;

pub async fn h_dht_stats(State(state): State<ApiState>) -> Result<impl IntoResponse> {
    state.api.api_dht_stats().map(axum::Json)
}

pub async fn h_dht_table(State(state): State<ApiState>) -> Result<impl IntoResponse> {
    state.api.api_dht_table().map(axum::Json)
}
