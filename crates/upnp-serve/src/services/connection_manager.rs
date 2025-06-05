use axum::{body::Bytes, extract::State, response::IntoResponse};
use bstr::BStr;
use http::{HeaderMap, StatusCode, header::CONTENT_TYPE};
use tracing::{debug, trace};

use crate::{
    constants::CONTENT_TYPE_XML_UTF8, state::UnpnServerState, subscriptions::SubscribeRequest,
};

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

    let not_implemented = || StatusCode::NOT_IMPLEMENTED.into_response();

    match action.as_ref() {
        SOAP_ACTION_GET_PROTOCOL_INFO => (
            [(CONTENT_TYPE, CONTENT_TYPE_XML_UTF8)],
            include_str!("../resources/templates/connection_manager/control/get_protocol_info.xml"),
        )
            .into_response(),

        SOAP_ACTION_GET_CURRENT_CONNECTION_INFO => (
            [(CONTENT_TYPE, CONTENT_TYPE_XML_UTF8)],
            include_str!(
                "../resources/templates/connection_manager/control/get_current_connection_info.xml"
            ),
        )
            .into_response(),
        SOAP_ACTION_GET_CURRENT_CONNECTION_IDS => (
            [(CONTENT_TYPE, CONTENT_TYPE_XML_UTF8)],
            include_str!(
                "../resources/templates/connection_manager/control/get_current_connection_ids.xml"
            ),
        )
            .into_response(),
        SOAP_ACTION_PREPARE_FOR_CONNECTION => not_implemented(),
        SOAP_ACTION_CONNECTION_COMPLETE => not_implemented(),
        _ => StatusCode::BAD_REQUEST.into_response(),
    }
}

pub(crate) async fn subscribe_http_handler(
    State(state): State<UnpnServerState>,
    request: axum::extract::Request,
) -> impl IntoResponse {
    let req = match SubscribeRequest::parse(request) {
        Ok(sub) => sub,
        Err(err) => return err,
    };

    let resp = state.handle_connection_manager_subscription_request(&req);
    crate::subscriptions::subscription_into_response(&req, resp)
}
