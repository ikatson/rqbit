use anyhow::Context;
use axum::{extract::State, response::IntoResponse};
use bencode::AsDisplay;
use buffers::ByteBuf;
use http::{HeaderMap, HeaderValue, StatusCode};
use librqbit_core::magnet::Magnet;

use super::ApiState;
use crate::{
    AddTorrent, AddTorrentOptions, ListOnlyResponse, api::Result, api::TorrentIdOrHash,
    http_api::timeout::Timeout,
};

pub async fn h_resolve_magnet(
    State(state): State<ApiState>,
    Timeout(timeout): Timeout<600_000, 3_600_000>,
    inp_headers: HeaderMap,
    url: String,
) -> Result<impl IntoResponse> {
    if (url.starts_with("magnet:") || url.len() == 40)
        && let Ok(magnet) = Magnet::parse(&url)
    {
        let info_hash = match (magnet.as_id20(), magnet.as_id32()) {
            (Some(id20), _) => id20,
            (None, Some(id32)) => id32.truncate_for_dht(),
            (None, None) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "magnet link didn't contain a BTv1 or BTv2 infohash",
                )
                    .into());
            }
        };

        if let Some(handle) = state.api.session().get(TorrentIdOrHash::Hash(info_hash)) {
            let (info, content) =
                handle.with_metadata(|r| (r.info.clone(), r.torrent_bytes.clone()))?;
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

            if let Some(name) = info.name()
                && let Ok(h) =
                    HeaderValue::from_str(&format!("attachment; filename=\"{name}.torrent\""))
            {
                headers.insert("Content-Disposition", h);
            }
            return Ok((headers, content).into_response());
        }
    }

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

    if let Some(name) = info.name()
        && let Ok(h) = HeaderValue::from_str(&format!("attachment; filename=\"{name}.torrent\""))
    {
        headers.insert("Content-Disposition", h);
    }
    Ok((headers, content).into_response())
}

#[cfg(all(test, feature = "http-api"))]
mod tests {
    use super::h_resolve_magnet;
    use crate::{
        AddTorrent, AddTorrentOptions, CreateTorrentOptions, Session, SessionOptions,
        create_torrent,
        http_api::HttpApi,
        spawn_utils::BlockingSpawner,
        tests::test_util::{create_default_random_dir_with_torrents, setup_test_logging},
    };
    use axum::{extract::State, http::StatusCode, response::IntoResponse};
    use http::HeaderMap;
    use librqbit_core::{
        magnet::Magnet,
        torrent_metainfo::{TorrentVersion, torrent_from_bytes},
    };
    use std::sync::Arc;
    #[tokio::test]
    async fn test_resolve_magnet_v2_only() {
        setup_test_logging();

        let tempdir =
            create_default_random_dir_with_torrents(1, 256 * 1024, Some("rqbit_http_magnet_v2"));
        let torrent = create_torrent(
            tempdir.path(),
            CreateTorrentOptions {
                version: Some(TorrentVersion::V2Only),
                piece_length: Some(65536),
                ..Default::default()
            },
            &BlockingSpawner::new(1),
        )
        .await
        .unwrap();
        let magnet = torrent.as_magnet().to_string();
        let expected_id32 = Magnet::parse(&magnet).unwrap().as_id32().unwrap();
        let torrent_bytes = torrent.as_bytes().unwrap();

        let session = Session::new_with_opts(
            std::env::temp_dir().join("rqbit_http_magnet_session"),
            SessionOptions {
                disable_dht: true,
                disable_local_service_discovery: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        session
            .add_torrent(
                AddTorrent::TorrentFileBytes(torrent_bytes),
                Some(AddTorrentOptions {
                    overwrite: true,
                    ..Default::default()
                }),
            )
            .await
            .unwrap();

        #[cfg(feature = "tracing-subscriber-utils")]
        let api = crate::api::Api::new(session, None, None);
        #[cfg(not(feature = "tracing-subscriber-utils"))]
        let api = crate::api::Api::new(session, None);

        let http_api = Arc::new(HttpApi::new(api, None));
        let response = h_resolve_magnet(
            State(http_api),
            crate::http_api::timeout::Timeout(std::time::Duration::from_secs(60)),
            HeaderMap::new(),
            magnet,
        )
        .await
        .unwrap()
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed = torrent_from_bytes(body.as_ref()).unwrap();
        assert_eq!(parsed.version(), Some(TorrentVersion::V2Only));
        assert_eq!(parsed.info_hash_v2.unwrap(), expected_id32);
    }
}
