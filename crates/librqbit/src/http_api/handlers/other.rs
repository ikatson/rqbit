use anyhow::Context;
use axum::{extract::State, response::IntoResponse};
use bencode::AsDisplay;
use buffers::ByteBuf;
use http::{HeaderMap, HeaderValue, StatusCode};

use super::ApiState;
use crate::{
    AddTorrent, AddTorrentOptions, ListOnlyResponse, api::Result, http_api::timeout::Timeout,
};

pub async fn h_resolve_magnet(
    State(state): State<ApiState>,
    Timeout(timeout): Timeout<600_000, 3_600_000>,
    inp_headers: HeaderMap,
    url: String,
) -> Result<impl IntoResponse> {
    let added = tokio::time::timeout(
        timeout,
        state.api.session().add_torrent(
            AddTorrent::from_url(&url),
            Some(AddTorrentOptions {
                list_only: true,
                ..Default::default()
            }),
        ),
    )
    .await
    .context("timeout")??;

    let (info, content) = match added {
        crate::AddTorrentResponse::AlreadyManaged(_, handle) => {
            handle.with_metadata(|r| (r.info.clone(), r.torrent_bytes.clone()))?
        }
        crate::AddTorrentResponse::ListOnly(ListOnlyResponse {
            info,
            torrent_bytes,
            ..
        }) => (info, torrent_bytes),
        crate::AddTorrentResponse::Added(_, _) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "bug: torrent was added to session, but shouldn't have been",
            )
                .into());
        }
    };

    let mut headers = HeaderMap::new();

    if inp_headers
        .get("Accept")
        .and_then(|v| std::str::from_utf8(v.as_bytes()).ok())
        == Some("application/json")
    {
        let data = bencode::dyn_from_bytes::<AsDisplay<ByteBuf>>(&content)
            .map_err(|e| {
                tracing::trace!("error decoding .torrent file content: {e:#}");
                e.into_kind()
            })
            .context("error decoding .torrent file content")?;
        let data = serde_json::to_string(&data).context("error serializing")?;
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));
        return Ok((headers, data).into_response());
    }

    headers.insert(
        "Content-Type",
        HeaderValue::from_static("application/x-bittorrent"),
    );

    if let Some(name) = info.name.as_ref() {
        if let Ok(name) = std::str::from_utf8(name.as_ref()) {
            if let Ok(h) =
                HeaderValue::from_str(&format!("attachment; filename=\"{}.torrent\"", name))
            {
                headers.insert("Content-Disposition", h);
            }
        }
    }
    Ok((headers, content).into_response())
}
