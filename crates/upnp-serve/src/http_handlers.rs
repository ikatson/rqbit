use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::State,
    response::IntoResponse,
    routing::{get, post},
};
use bstr::BStr;
use http::{HeaderMap, StatusCode};
use tracing::trace;

use crate::{
    constants::SOAP_ACTION_CONTENT_DIRECTORY_BROWSE,
    state::{ContentDirectoryBrowseProvider, UnpnServerState, UnpnServerStateInner},
    templates::{render_root_description_xml, RootDescriptionInputs},
};

async fn description_xml(State(state): State<UnpnServerState>) -> impl IntoResponse {
    state.rendered_root_description.clone()
}

async fn generate_content_directory_control_response(
    headers: HeaderMap,
    State(state): State<UnpnServerState>,
    body: Bytes,
) -> impl IntoResponse {
    let body = BStr::new(&body);
    trace!(?body, "received control request");
    let action = headers.get("soapaction").map(|v| v.as_bytes());
    if action != Some(SOAP_ACTION_CONTENT_DIRECTORY_BROWSE) {
        return (StatusCode::NOT_IMPLEMENTED, "").into_response();
    }

    crate::templates::render_content_directory_browse(state.provider.browse()).into_response()
}

pub fn make_router(
    friendly_name: String,
    http_prefix: String,
    upnp_usn: String,
    server_header_string: String,
    port: u16,
    browse_provider: Box<dyn ContentDirectoryBrowseProvider>,
) -> anyhow::Result<axum::Router<UnpnServerState>> {
    let root_desc = render_root_description_xml(&RootDescriptionInputs {
        friendly_name: &friendly_name,
        manufacturer: "rqbit developers",
        model_name: "1.0.0",
        unique_id: &upnp_usn,
        http_prefix: &http_prefix,
    });

    let state = Arc::new(UnpnServerStateInner {
        usn: upnp_usn,
        friendly_name,
        server_header_string,
        port,
        rendered_root_description: root_desc.into(),
        provider: browse_provider,
    });

    let app = axum::Router::new()
        .route("/description.xml", get(description_xml))
        .route(
            "/scpd/ContentDirectory.xml",
            get(|| async { include_str!("resources/scpd_content_directory.xml") }),
        )
        .route(
            "/scpd/ConnectionManager.xml",
            get(|| async { include_str!("resources/scpd_connection_manager.xml") }),
        )
        .route(
            "/control/ContentDirectory",
            post(generate_content_directory_control_response),
        )
        .route(
            "/control/ConnectionManager",
            post(|| async { (StatusCode::NOT_IMPLEMENTED, "") }),
        )
        .with_state(state);

    Ok(app)
}
