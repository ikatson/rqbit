use std::sync::atomic::Ordering;

use axum::{body::Bytes, extract::State, response::IntoResponse};
use browse::response::ItemOrContainer;
use bstr::BStr;
use http::{HeaderMap, StatusCode, header::CONTENT_TYPE};
use tracing::{debug, trace};

use crate::{
    constants::{
        CONTENT_TYPE_XML_UTF8, SOAP_ACTION_CONTENT_DIRECTORY_BROWSE,
        SOAP_ACTION_GET_SYSTEM_UPDATE_ID,
    },
    state::UnpnServerState,
};

pub mod browse {
    pub mod request {
        use anyhow::Context;
        use serde_derive::Deserialize;

        #[derive(Deserialize)]
        struct Envelope {
            #[serde(rename = "Body")]
            body: Body,
        }

        #[derive(Deserialize)]
        struct Body {
            #[serde(rename = "Browse")]
            browse: ContentDirectoryControlRequest,
        }

        #[derive(Deserialize, PartialEq, Eq, Debug)]
        pub enum BrowseFlag {
            BrowseDirectChildren,
            BrowseMetadata,
        }

        #[derive(Deserialize, Debug)]
        pub struct ContentDirectoryControlRequest {
            #[serde(rename = "ObjectID")]
            pub object_id: usize,
            #[serde(rename = "BrowseFlag")]
            pub browse_flag: BrowseFlag,
            #[serde(rename = "StartingIndex", default)]
            pub starting_index: usize,
            #[serde(rename = "RequestedCount", default)]
            pub requested_count: usize,
        }

        impl ContentDirectoryControlRequest {
            pub fn parse(s: &str) -> anyhow::Result<Self> {
                let envelope: Envelope =
                    quick_xml::de::from_str(s).context("error deserializing")?;
                Ok(envelope.body.browse)
            }
        }
    }

    pub mod response {
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct Container {
            pub id: usize,
            // Parent id is None only for the root container.
            // The only way to see the root container is BrowseMetadata on ObjectID=0
            pub parent_id: Option<usize>,
            pub children_count: Option<usize>,
            pub title: String,
        }

        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct Item {
            pub id: usize,
            pub parent_id: usize,
            pub title: String,
            pub mime_type: Option<mime_guess::Mime>,
            pub url: String,
            pub size: u64,
        }

        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum ItemOrContainer {
            Container(Container),
            Item(Item),
        }

        pub(crate) fn render(items: impl IntoIterator<Item = ItemOrContainer>) -> String {
            fn item_or_container(item_or_container: &ItemOrContainer) -> Option<String> {
                fn item(item: &Item) -> Option<String> {
                    let mime = item.mime_type.as_ref()?;
                    let upnp_class = match mime.type_().as_str() {
                        "video" => "object.item.videoItem",
                        _ => return None,
                    };
                    let mime = mime.to_string();

                    Some(format!(
                        include_str!(
                            "../resources/templates/content_directory/control/browse/item.tmpl.xml"
                        ),
                        id = item.id,
                        parent_id = item.parent_id,
                        mime_type = mime,
                        url = item.url,
                        upnp_class = upnp_class,
                        title = item.title,
                        size = item.size
                    ))
                }

                fn container(item: &Container) -> String {
                    let child_count_tag = match item.children_count {
                        Some(cc) => format!("childCount=\"{cc}\""),
                        None => String::new(),
                    };
                    format!(
                        include_str!(
                            "../resources/templates/content_directory/control/browse/container.tmpl.xml"
                        ),
                        id = item.id,
                        parent_id = item.parent_id.map(|p| p as isize).unwrap_or(-1),
                        title = item.title,
                        childCountTag = child_count_tag
                    )
                }

                match item_or_container {
                    ItemOrContainer::Container(c) => Some(container(c)),
                    ItemOrContainer::Item(i) => item(i),
                }
            }

            struct Envelope<'a> {
                items: &'a str,
                number_returned: usize,
                total_matches: usize,
                update_id: u64,
            }

            fn render_response(envelope: &Envelope<'_>) -> String {
                let items_encoded = format!(
                    r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"
                xmlns:dc="http://purl.org/dc/elements/1.1/"
                xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">
      {items}
    </DIDL-Lite>"#,
                    items = envelope.items
                );

                // This COULD have been done with CDATA, but some Samsung TVs don't like that, they want
                // escaped XML instead.
                let items_encoded = quick_xml::escape::escape(items_encoded);

                format!(
                    include_str!(
                        "../resources/templates/content_directory/control/browse/response.tmpl.xml"
                    ),
                    items_encoded = items_encoded,
                    number_returned = envelope.number_returned,
                    total_matches = envelope.total_matches,
                    update_id = envelope.update_id
                )
            }

            let all_items = items
                .into_iter()
                .filter_map(|item| item_or_container(&item))
                .collect::<Vec<_>>();
            let total = all_items.len();
            let all_items = all_items.join("");

            use std::time::{SystemTime, UNIX_EPOCH};
            let update_id = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);

            render_response(&Envelope {
                items: &all_items,
                number_returned: total,
                total_matches: total,
                update_id,
            })
        }
    }
}

pub mod get_system_update_id {
    pub(crate) fn render_notify(update_id: u64) -> String {
        format!(
            include_str!(
                "../resources/templates/content_directory/subscriptions/system_update_id.tmpl.xml"
            ),
            system_update_id = update_id
        )
    }

    pub(crate) fn render_response(update_id: u64) -> String {
        format!(
            include_str!(
                "../resources/templates/content_directory/control/get_system_update_id/response.tmpl.xml"
            ),
            id = update_id
        )
    }
}

pub mod subscription {
    use axum::{extract::State, response::IntoResponse};
    use http::Method;

    use crate::{state::UnpnServerState, subscriptions::SubscribeRequest};

    pub(crate) async fn subscribe_http_handler(
        State(state): State<UnpnServerState>,
        request: axum::extract::Request,
    ) -> impl IntoResponse {
        let req = match SubscribeRequest::parse(request) {
            Ok(sub) => sub,
            Err(err) => return err,
        };

        let resp = state.handle_content_directory_subscription_request(&req);
        crate::subscriptions::subscription_into_response(&req, resp)
    }

    pub async fn notify_system_id_update(
        url: &url::Url,
        sid: &str,
        seq: u64,
        system_update_id: u64,
    ) -> anyhow::Result<()> {
        // NOTIFY /callback_path HTTP/1.1
        // CONTENT-TYPE: text/xml; charset="utf-8"
        // NT: upnp:event
        // NTS: upnp:propchange
        // SID: uuid:<Subscription ID>
        // SEQ: <sequence number>
        //
        let body = super::get_system_update_id::render_notify(system_update_id);

        let resp = reqwest::Client::builder()
            .build()?
            .request(Method::from_bytes(b"NOTIFY")?, url.clone())
            .header("Content-Type", r#"text/xml; charset="utf-8""#)
            .header("NT", "upnp:event")
            .header("NTS", "upnp:propchange")
            .header("SID", sid)
            .header("SEQ", seq.to_string())
            .body(body)
            .send()
            .await?;

        if !resp.status().is_success() {
            anyhow::bail!("{:?}", resp.status())
        }
        Ok(())
    }
}

pub(crate) async fn http_handler(
    headers: HeaderMap,
    State(state): State<UnpnServerState>,
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
    match action.as_ref() {
        SOAP_ACTION_CONTENT_DIRECTORY_BROWSE => {
            let http_host = headers
                .get("host")
                .and_then(|h| std::str::from_utf8(h.as_bytes()).ok());
            let http_hostname = match http_host {
                Some(h) => h,
                None => return StatusCode::BAD_REQUEST.into_response(),
            };

            let body = match std::str::from_utf8(body) {
                Ok(body) => body,
                Err(_) => return (StatusCode::BAD_REQUEST, "cannot parse request").into_response(),
            };

            let request = match browse::request::ContentDirectoryControlRequest::parse(body) {
                Ok(req) => req,
                Err(e) => {
                    debug!(error=?e, "error parsing XML");
                    return (StatusCode::BAD_REQUEST, "cannot parse request").into_response();
                }
            };

            use browse::request::BrowseFlag;

            match request.browse_flag {
                BrowseFlag::BrowseDirectChildren => (
                    [(CONTENT_TYPE, CONTENT_TYPE_XML_UTF8)],
                    browse::response::render(
                        state
                            .provider
                            .browse_direct_children(request.object_id, http_hostname),
                    ),
                )
                    .into_response(),
                BrowseFlag::BrowseMetadata => (
                    [(CONTENT_TYPE, CONTENT_TYPE_XML_UTF8)],
                    browse::response::render(
                        state
                            .provider
                            .browse_metadata(request.object_id, http_hostname),
                    ),
                )
                    .into_response(),
            }
        }
        SOAP_ACTION_GET_SYSTEM_UPDATE_ID => {
            let update_id = state.system_update_id.load(Ordering::Relaxed);
            (
                [(CONTENT_TYPE, CONTENT_TYPE_XML_UTF8)],
                get_system_update_id::render_response(update_id),
            )
                .into_response()
        }
        _ => {
            debug!(?action, "unsupported ContentDirectory action");
            (StatusCode::NOT_IMPLEMENTED, "").into_response()
        }
    }
}

pub trait ContentDirectoryBrowseProvider: Send + Sync {
    fn browse_direct_children(&self, parent_id: usize, http_hostname: &str)
    -> Vec<ItemOrContainer>;
    fn browse_metadata(&self, object_id: usize, http_hostname: &str) -> Vec<ItemOrContainer>;
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_parse_content_directory_request() {
        use super::browse::request::{BrowseFlag, ContentDirectoryControlRequest};

        let s = include_str!("../resources/test/ContentDirectoryControlExampleRequest.xml");
        let req = ContentDirectoryControlRequest::parse(s).unwrap();
        assert_eq!(req.object_id, 5);
        assert_eq!(req.browse_flag, BrowseFlag::BrowseDirectChildren)
    }
}
