use std::{sync::atomic::Ordering, time::Duration};

use anyhow::Context;
use axum::{
    body::Bytes,
    extract::State,
    handler::HandlerWithoutStateExt,
    response::IntoResponse,
    routing::{get, post},
};
use bstr::BStr;
use http::{header::CONTENT_TYPE, HeaderMap, HeaderName, StatusCode};
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

use crate::{
    constants::{
        CONTENT_TYPE_XML_UTF8, SOAP_ACTION_CONTENT_DIRECTORY_BROWSE,
        SOAP_ACTION_GET_SYSTEM_UPDATE_ID,
    },
    state::{UnpnServerState, UpnpServerStateInner},
    templates::{
        render_content_directory_browse, render_content_directory_control_get_system_update_id,
        render_root_description_xml, RootDescriptionInputs,
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
    let action = headers.get("soapaction").map(|v| BStr::new(v.as_bytes()));
    let action = match action {
        Some(action) => action,
        None => {
            debug!("missing SOAPACTION header");
            return (StatusCode::BAD_REQUEST, "").into_response();
        }
    };
    trace!(?action);
    match action.as_ref() {
        SOAP_ACTION_CONTENT_DIRECTORY_BROWSE => {
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
                [(CONTENT_TYPE, CONTENT_TYPE_XML_UTF8)],
                render_content_directory_browse(
                    state.provider.browse_direct_children(request.object_id),
                ),
            )
                .into_response()
        }
        SOAP_ACTION_GET_SYSTEM_UPDATE_ID => {
            let update_id = state.system_update_id.load(Ordering::Relaxed);
            (
                [(CONTENT_TYPE, CONTENT_TYPE_XML_UTF8)],
                render_content_directory_control_get_system_update_id(update_id),
            )
                .into_response()
        }
        _ => {
            debug!(?action, "unsupported ContentDirectory action");
            (StatusCode::NOT_IMPLEMENTED, "").into_response()
        }
    }
}

async fn subscription(
    State(state): State<UnpnServerState>,
    request: axum::extract::Request,
) -> impl IntoResponse {
    if request.method().as_str() != "SUBSCRIBE" {
        return (StatusCode::METHOD_NOT_ALLOWED, "").into_response();
    }

    let (parts, _body) = request.into_parts();
    dbg!(&parts.headers);
    trace!(?parts.headers, "subscription request");
    let is_event = parts
        .headers
        .get(HeaderName::from_static("nt"))
        .map(|v| v.as_bytes() == b"upnp:event")
        .unwrap_or_default();
    if !is_event {
        return (StatusCode::BAD_REQUEST, "expected NT: upnp:event header").into_response();
    }

    let callback = parts
        .headers
        .get(HeaderName::from_static("callback"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim_matches(|c| c == '>' || c == '<'))
        .and_then(|u| url::Url::parse(u).ok());
    let callback = match callback {
        Some(c) => c,
        None => return (StatusCode::BAD_REQUEST, "callback not provided").into_response(),
    };
    let subscription_id = parts
        .headers
        .get(HeaderName::from_static("sid"))
        .and_then(|v| v.to_str().ok());

    let timeout = parts
        .headers
        .get(HeaderName::from_static("timeout"))
        .and_then(|v| v.to_str().ok())
        .and_then(|t| t.strip_prefix("Second-"))
        .and_then(|t| t.parse::<u16>().ok())
        .map(|t| Duration::from_secs(t as u64));

    const DEFAULT_TIMEOUT: Duration = Duration::from_secs(1800);

    let timeout = timeout.unwrap_or(DEFAULT_TIMEOUT);

    if let Some(sid) = subscription_id {
        match state.renew_subscription(sid, timeout) {
            Ok(()) => (
                StatusCode::OK,
                [
                    ("SID", sid.to_owned()),
                    ("TIMEOUT", format!("Seconds-{}", timeout.as_secs())),
                ],
            )
                .into_response(),
            Err(e) => {
                warn!(sid, error=?e, "error renewing subscription");
                StatusCode::NOT_FOUND.into_response()
            }
        }
    } else {
        match state.new_subscription(callback, timeout) {
            Ok(sid) => (
                StatusCode::OK,
                [
                    ("SID", sid),
                    ("TIMEOUT", format!("Seconds-{}", timeout.as_secs())),
                ],
            )
                .into_response(),
            Err(e) => {
                warn!(error=?e, "error creating subscription");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
    }
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

    let sub_handler = {
        let state = state.clone();
        move |request: axum::extract::Request| async move {
            subscription(State(state.clone()), request).await
        }
    };

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
        .route_service("/subscribe", sub_handler.into_service())
        .with_state(state);

    Ok(app)
}
