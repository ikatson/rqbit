use anyhow::Context;
use axum::{
    extract::{Path, State},
    response::IntoResponse,
};
use http::{HeaderMap, StatusCode};
use itertools::Itertools;

use super::ApiState;
use crate::{
    ManagedTorrent,
    api::{Result, TorrentIdOrHash},
    api_error::WithStatus,
};

fn torrent_playlist_items(handle: &ManagedTorrent) -> Result<Vec<(usize, String)>> {
    let mut playlist_items = handle
        .metadata
        .load()
        .as_ref()
        .context("torrent metadata not resolved")?
        .info
        .iter_file_details()
        .enumerate()
        .filter_map(|(file_idx, file_details)| {
            let filename = file_details.filename.to_vec().join("/");
            let is_playable = mime_guess::from_path(&filename)
                .first()
                .map(|mime| {
                    mime.type_() == mime_guess::mime::VIDEO
                        || mime.type_() == mime_guess::mime::AUDIO
                })
                .unwrap_or(false);
            if is_playable {
                let filename = urlencoding::encode(&filename);
                Some((file_idx, filename.into_owned()))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    playlist_items.sort_by(|left, right| left.1.cmp(&right.1));
    Ok(playlist_items)
}

fn get_host(headers: &HeaderMap) -> Result<&str> {
    headers
        .get("host")
        .ok_or("Missing host header")
        .and_then(|h| h.to_str().map_err(|_| "hostname is not a string"))
        .with_status(StatusCode::BAD_REQUEST)
}

fn build_playlist_content<I: IntoIterator<Item = (TorrentIdOrHash, usize, String)>>(
    host: &str,
    it: I,
) -> impl IntoResponse + use<I> {
    let body = it
        .into_iter()
        .map(|(torrent_idx, file_idx, filename)| {
            // TODO: add #EXTINF:{duration} and maybe codecs ?
            format!("http://{host}/torrents/{torrent_idx}/stream/{file_idx}/{filename}")
        })
        .join("\r\n");
    (
        [
            ("Content-Type", "application/mpegurl; charset=utf-8"),
            (
                "Content-Disposition",
                "attachment; filename=\"rqbit-playlist.m3u8\"",
            ),
        ],
        format!("#EXTM3U\r\n{body}"), // https://en.wikipedia.org/wiki/M3U
    )
}

pub async fn h_torrent_playlist(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(idx): Path<TorrentIdOrHash>,
) -> Result<impl IntoResponse> {
    let host = get_host(&headers)?;
    let playlist_items = torrent_playlist_items(&*state.api.mgr_handle(idx)?)?;
    Ok(build_playlist_content(
        host,
        playlist_items
            .into_iter()
            .map(move |(file_idx, filename)| (idx, file_idx, filename)),
    ))
}

pub async fn h_global_playlist(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse> {
    let host = get_host(&headers)?;
    let all_items = state.api.session().with_torrents(|torrents| {
        torrents
            .filter_map(|(torrent_idx, handle)| {
                torrent_playlist_items(handle)
                    .map(move |items| {
                        items.into_iter().map(move |(file_idx, filename)| {
                            (torrent_idx.into(), file_idx, filename)
                        })
                    })
                    .ok()
            })
            .flatten()
            .collect::<Vec<_>>()
    });
    Ok(build_playlist_content(host, all_items))
}
