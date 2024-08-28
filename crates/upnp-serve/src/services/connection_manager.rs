use axum::{body::Bytes, extract::State, response::IntoResponse};
use bstr::BStr;
use http::{HeaderMap, StatusCode};
use tracing::{debug, trace, warn};

use crate::{state::UnpnServerState, subscriptions::SubscribeRequest};

pub const SOAP_ACTION_GET_PROTOCOL_INFO: &[u8] =
    b"\"urn:schemas-upnp-org:service:ConnectionManager:1#GetProtocolInfo\"";

pub const SOAP_ACTION_CONNECTION_COMPLETE: &[u8] =
    b"\"urn:schemas-upnp-org:service:ConnectionManager:1#ConnectionComplete\"";

pub const SOAP_ACTION_GET_CURRENT_CONNECTION_IDS: &[u8] =
    b"\"urn:schemas-upnp-org:service:ConnectionManager:1#GetCurrentConnectionIDs\"";

pub const SOAP_ACTION_GET_CURRENT_CONNECTION_INFO: &[u8] =
    b"\"urn:schemas-upnp-org:service:ConnectionManager:1#GetCurrentConnectionInfo\"";

pub const SOAP_ACTION_PREPARE_FOR_CONNECTION: &[u8] =
    b"\"urn:schemas-upnp-org:service:ConnectionManager:1#PrepareForConnection\"";

pub(crate) async fn http_handler(
    headers: HeaderMap,
    State(_state): State<UnpnServerState>,
    body: Bytes,
) -> impl IntoResponse {
    let body = BStr::new(&body);
    let action = headers.get("soapaction").map(|v| BStr::new(v.as_bytes()));
    trace!(?body, ?action, "received control request");
    let action = match action {
        Some(action) => action,
        None => {
            debug!("missing SOAPACTION header");
            return (StatusCode::BAD_REQUEST, "").into_response();
        }
    };

    let not_implemented = StatusCode::NOT_IMPLEMENTED.into_response();

    match action.as_ref() {
        SOAP_ACTION_GET_PROTOCOL_INFO => not_implemented,
        SOAP_ACTION_CONNECTION_COMPLETE => not_implemented,
        SOAP_ACTION_GET_CURRENT_CONNECTION_INFO => not_implemented,
        SOAP_ACTION_GET_CURRENT_CONNECTION_IDS => not_implemented,
        SOAP_ACTION_PREPARE_FOR_CONNECTION => not_implemented,
        _ => StatusCode::BAD_REQUEST.into_response(),
    }
}

pub(crate) async fn subscribe_http_handler(
    State(state): State<UnpnServerState>,
    request: axum::extract::Request,
) -> impl IntoResponse {
    let SubscribeRequest {
        callback,
        subscription_id,
        timeout,
    } = match SubscribeRequest::parse(request) {
        Ok(sub) => sub,
        Err(e) => return e,
    };

    if let Some(sid) = subscription_id {
        match state.renew_connection_manager_subscription(&sid, timeout) {
            Ok(()) => (
                StatusCode::OK,
                [
                    ("SID", sid.to_owned()),
                    ("TIMEOUT", format!("Second-{}", timeout.as_secs())),
                ],
            )
                .into_response(),
            Err(e) => {
                warn!(sid, error=?e, "error renewing subscription");
                StatusCode::NOT_FOUND.into_response()
            }
        }
    } else {
        match state.new_connection_manager_subscription(callback, timeout) {
            Ok(sid) => (
                StatusCode::OK,
                [
                    ("SID", sid),
                    ("TIMEOUT", format!("Second-{}", timeout.as_secs())),
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
