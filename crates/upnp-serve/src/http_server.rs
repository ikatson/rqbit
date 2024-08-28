use anyhow::Context;
use axum::{
    extract::State,
    handler::HandlerWithoutStateExt,
    response::IntoResponse,
    routing::{get, post},
};
use http::{header::CONTENT_TYPE, StatusCode};
use tokio_util::sync::CancellationToken;

use crate::{
    constants::CONTENT_TYPE_XML_UTF8,
    services::content_directory::ContentDirectoryBrowseProvider,
    state::{UnpnServerState, UpnpServerStateInner},
};

async fn description_xml(State(state): State<UnpnServerState>) -> impl IntoResponse {
    (
        [(CONTENT_TYPE, CONTENT_TYPE_XML_UTF8)],
        state.rendered_root_description.clone(),
    )
}

pub struct RootDescriptionInputs<'a> {
    pub friendly_name: &'a str,
    pub manufacturer: &'a str,
    pub model_name: &'a str,
    pub unique_id: &'a str,
    pub http_prefix: &'a str,
}

pub fn render_root_description_xml(input: &RootDescriptionInputs<'_>) -> String {
    format!(
        include_str!("resources/templates/root_desc.tmpl.xml"),
        friendly_name = input.friendly_name,
        manufacturer = input.manufacturer,
        model_name = input.model_name,
        unique_id = input.unique_id,
        http_prefix = input.http_prefix
    )
}

pub fn make_router(
    friendly_name: String,
    http_prefix: String,
    upnp_usn: String,
    browse_provider: Box<dyn ContentDirectoryBrowseProvider>,
    cancellation_token: CancellationToken,
) -> anyhow::Result<axum::Router> {
    let root_desc = render_root_description_xml(&RootDescriptionInputs {
        friendly_name: &friendly_name,
        manufacturer: "rqbit developers",
        model_name: "1.0.0",
        unique_id: &upnp_usn,
        http_prefix: &http_prefix,
    });

    let state = UpnpServerStateInner::new(root_desc.into(), browse_provider, cancellation_token)
        .context("error creating UPNP server")?;

    let content_dir_sub_handler = {
        let state = state.clone();
        move |request: axum::extract::Request| async move {
            crate::services::content_directory::subscription::subscribe_http_handler(
                State(state.clone()),
                request,
            )
            .await
        }
    };

    let app = axum::Router::new()
        .route("/description.xml", get(description_xml))
        .route(
            "/scpd/ContentDirectory.xml",
            get(|| async { include_str!("resources/templates/content_directory/scpd.xml") }),
        )
        .route(
            "/scpd/ConnectionManager.xml",
            get(|| async { include_str!("resources/templates/connection_manager/scpd.xml") }),
        )
        .route(
            "/control/ContentDirectory",
            post(crate::services::content_directory::http_handler),
        )
        .route(
            "/control/ConnectionManager",
            post(|| async { (StatusCode::NOT_IMPLEMENTED, "") }),
        )
        .route_service(
            "/subscribe/ContentDirectory",
            content_dir_sub_handler.into_service(),
        )
        .route(
            "/subscribe/ConnectionManager",
            post(|| async { (StatusCode::NOT_IMPLEMENTED, "") }),
        )
        .with_state(state);

    Ok(app)
}
