use anyhow::Context;
use serde::Deserialize;

use crate::{http_api::TorrentAddQueryParams, session::AddTorrentOptions};

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
    let body = r.text().await.with_context(|| {
        format!(
            "cannot read response body for request to {} ({})",
            url, status,
        )
    })?;
    anyhow::bail!("{} -> {}: {}", url, status, body)
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
    pub fn new(url: &str) -> anyhow::Result<Self> {
        Ok(Self {
            base_url: reqwest::Url::parse(url)?,
            client: reqwest::ClientBuilder::new().build()?,
        })
    }

    pub fn base_url(&self) -> &reqwest::Url {
        &self.base_url
    }

    pub async fn validate_rqbit_server(&self) -> anyhow::Result<()> {
        let response = self.client.get(self.base_url.clone()).send().await?;
        let root: ApiRoot = json_response(response).await?;
        if root.server == "rqbit" {
            return Ok(());
        }
        anyhow::bail!("not an rqbit server at {}", &self.base_url)
    }

    pub async fn add_torrent(
        &self,
        torrent: &str,
        opts: Option<AddTorrentOptions>,
    ) -> anyhow::Result<crate::http_api::ApiAddTorrentResponse> {
        let opts = opts.unwrap_or_default();
        let params = TorrentAddQueryParams {
            overwrite: Some(opts.overwrite),
            only_files_regex: opts.only_files_regex,
            output_folder: opts.output_folder,
            sub_folder: opts.sub_folder,
            list_only: Some(opts.list_only),
        };
        let qs = serde_urlencoded::to_string(&params).unwrap();
        let url = format!("{}torrents?{}", &self.base_url, qs);
        let response = check_response(
            self.client
                .post(&url)
                .body(torrent.to_owned())
                .send()
                .await?,
        )
        .await?;
        json_response(response).await
    }
}
