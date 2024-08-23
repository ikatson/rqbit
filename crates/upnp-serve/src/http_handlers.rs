use std::{
    sync::{atomic::AtomicU64, Arc},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use axum::{
    body::Bytes,
    extract::State,
    handler::HandlerWithoutStateExt,
    response::IntoResponse,
    routing::{get, post},
};
use bstr::BStr;
use http::{
    header::{CACHE_CONTROL, CONTENT_TYPE},
    HeaderMap, HeaderName, StatusCode,
};
use tracing::{debug, trace};

use crate::{
    constants::{CONTENT_TYPE_XML_UTF8, SOAP_ACTION_CONTENT_DIRECTORY_BROWSE},
    state::{UnpnServerState, UnpnServerStateInner},
    templates::{
        render_content_directory_browse, render_root_description_xml, RootDescriptionInputs,
    },
    upnp_types::content_directory::{
        request::ContentDirectoryControlRequest, ContentDirectoryBrowseProvider,
    },
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

    let body = match std::str::from_utf8(body) {
        Ok(body) => body,
        Err(_) => return (StatusCode::BAD_REQUEST, "cannot parse request").into_response(),
    };

    let request = match ContentDirectoryControlRequest::parse(body) {
        Ok(req) => req,
        Err(e) => {
            debug!(error=?e, "error parsing XML");
            return (StatusCode::BAD_REQUEST, "cannot parse request").into_response();
        }
    };

    (
        [
            (CONTENT_TYPE, CONTENT_TYPE_XML_UTF8),
            (CACHE_CONTROL, "max-age=1"),
        ],
        render_content_directory_browse(state.provider.browse_direct_children(request.object_id)),
    )
        .into_response()
}

async fn subscription(request: axum::extract::Request) -> impl IntoResponse {
    if request.method().as_str() != "SUBSCRIBE" {
        return (StatusCode::METHOD_NOT_ALLOWED, "").into_response();
    }

    let is_event = request
        .headers()
        .get(HeaderName::from_static("nt"))
        .map(|v| v.as_bytes() == b"upnp:event")
        .unwrap_or_default();
    if !is_event {
        return (StatusCode::BAD_REQUEST, "expected NT: upnp:event header").into_response();
    }

    let callback = request
        .headers()
        .get(HeaderName::from_static("callback"))
        .and_then(|v| v.to_str().ok())
        .and_then(|u| url::Url::parse(u).ok());
    let callback = match callback {
        Some(c) => c,
        None => return (StatusCode::BAD_REQUEST, "callback not provided").into_response(),
    };
    let subscription_id = request
        .headers()
        .get(HeaderName::from_static("sid"))
        .and_then(|v| v.to_str().ok());
    let timeout = request
        .headers()
        .get(HeaderName::from_static("timeout"))
        .and_then(|v| v.to_str().ok())
        .and_then(|t| t.strip_prefix("Second-"))
        .and_then(|t| t.parse::<u16>().ok())
        .map(|t| Duration::from_secs(t as u64));

    let callback = match request.headers().get(HeaderName::from_static("callback")) {
        Some(v) => v.as_bytes(),
        None => todo!(),
    };

    todo!()
}

pub fn make_router(
    friendly_name: String,
    http_prefix: String,
    upnp_usn: String,
    browse_provider: Box<dyn ContentDirectoryBrowseProvider>,
) -> anyhow::Result<axum::Router> {
    let root_desc = render_root_description_xml(&RootDescriptionInputs {
        friendly_name: &friendly_name,
        manufacturer: "rqbit developers",
        model_name: "1.0.0",
        unique_id: &upnp_usn,
        http_prefix: &http_prefix,
    });

    let state = UnpnServerStateInner::new(root_desc.into(), browse_provider)
        .context("error creating UPNP server")?;

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
        .route_service("/subscribe", subscription.into_service())
        .with_state(state);

    Ok(app)
}
