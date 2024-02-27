use anyhow::Context;
use futures::{future::BoxFuture, FutureExt};
use serde::Deserialize;

use crate::{
    api::ApiAddTorrentResponse,
    http_api::TorrentAddQueryParams,
    session::{AddTorrent, AddTorrentOptions},
};

#[derive(Clone)]
pub struct HttpApiClient {
    client: reqwest::Client,
    base_url: reqwest::Url,
}

async fn check_response(r: reqwest::Response) -> anyhow::Result<reqwest::Response> {
    if r.status().is_success() {
        return Ok(r);
    }
    let status = r.status();
    let url = r.url().clone();
    let body = r
        .text()
        .await
        .with_context(|| format!("cannot read response body for request to {url} ({status})"))?;

    #[derive(Deserialize)]
    struct HumanReadableError<'a> {
        human_readable: Option<&'a str>,
    }

    let human_readable_internal_error = serde_json::from_str::<HumanReadableError<'_>>(&body)
        .ok()
        .and_then(|e| e.human_readable);
    let body_display = human_readable_internal_error.unwrap_or(&body);

    anyhow::bail!("{} -> {}: {}", url, status, body_display)
}

#[derive(Deserialize)]
struct ApiRoot {
    server: String,
}

async fn json_response<T: serde::de::DeserializeOwned + std::any::Any>(
    response: reqwest::Response,
) -> anyhow::Result<T> {
    let url = response.url().clone();
    let response = check_response(response).await?;
    let body = response.bytes().await?;
    let response: T = serde_json::from_slice(&body).with_context(|| {
        format!(
            "error deserializing response from {:?} as {:?}",
            url,
            std::any::type_name::<T>(),
        )
    })?;
    Ok(response)
}

impl HttpApiClient {
    #[inline(never)]
    pub fn new(url: &str) -> anyhow::Result<Self> {
        Ok(Self {
            base_url: reqwest::Url::parse(url)?,
            client: reqwest::ClientBuilder::new().build()?,
        })
    }

    pub fn base_url(&self) -> &reqwest::Url {
        &self.base_url
    }

    #[inline(never)]
    pub fn validate_rqbit_server(&self) -> BoxFuture<'_, anyhow::Result<()>> {
        async move {
            let response = self.client.get(self.base_url.clone()).send().await?;
            let root: ApiRoot = json_response(response).await?;
            if root.server == "rqbit" {
                return Ok(());
            }
            anyhow::bail!("not an rqbit server at {}", &self.base_url)
        }
        .boxed()
    }

    pub fn add_torrent<'a>(
        &'a self,
        torrent: AddTorrent<'a>,
        opts: Option<AddTorrentOptions>,
    ) -> BoxFuture<'a, anyhow::Result<ApiAddTorrentResponse>> {
        async move {
            let opts = opts.unwrap_or_default();
            let params = TorrentAddQueryParams {
                overwrite: Some(opts.overwrite),
                only_files_regex: opts.only_files_regex,
                only_files: None,
                output_folder: opts.output_folder,
                sub_folder: opts.sub_folder,
                list_only: Some(opts.list_only),
                ..Default::default()
            };
            let qs = serde_urlencoded::to_string(&params).unwrap();
            let url = format!("{}torrents?{}", &self.base_url, qs);
            let response = check_response(
                self.client
                    .post(&url)
                    .body(torrent.into_bytes())
                    .send()
                    .await?,
            )
            .await?;
            json_response(response).await
        }
        .boxed()
    }
}
