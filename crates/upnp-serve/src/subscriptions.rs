use crate::state::UpnpServerStateInner;
use anyhow::Context;
use axum::response::IntoResponse;
use http::{HeaderName, StatusCode};
use librqbit_core::spawn_utils::spawn_with_cancel;
use parking_lot::RwLock;
use std::{
    collections::HashMap,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::sync::{Notify, broadcast::error::RecvError};
use tracing::{Instrument, debug, error_span, trace, warn};

pub struct Subscription {
    #[allow(dead_code)]
    pub url: url::Url,
    pub seq: u64,
    pub timeout: Duration,
    pub refresh_notify: Arc<Notify>,
}

#[derive(Default)]
pub struct Subscriptions {
    subs: RwLock<HashMap<String, Subscription>>,
}

impl Subscriptions {
    pub fn add(&self, url: url::Url, timeout: Duration) -> (String, Arc<Notify>) {
        let sid = format!("uuid:{}", uuid::Uuid::new_v4());
        let notify = Arc::new(Notify::default());
        self.subs.write().insert(
            sid.clone(),
            Subscription {
                url,
                seq: 0,
                timeout,
                refresh_notify: notify.clone(),
            },
        );
        (sid, notify)
    }

    pub fn update_timeout(&self, sid: &str, timeout: Duration) -> anyhow::Result<()> {
        let mut g = self.subs.write();
        let s = g.get_mut(sid).context("no such subscription")?;
        s.timeout = timeout;
        s.refresh_notify.notify_waiters();
        Ok(())
    }

    pub fn next_seq(&self, sid: &str) -> anyhow::Result<u64> {
        let mut g = self.subs.write();
        let s = g.get_mut(sid).context("no such subscription")?;
        let id = s.seq;
        s.seq += 1;
        Ok(id)
    }

    pub fn get_timeout(&self, sid: &str) -> anyhow::Result<Duration> {
        let mut g = self.subs.write();
        let s = g.get_mut(sid).context("no such subscription")?;
        Ok(s.timeout)
    }

    pub fn remove(&self, sid: &str) -> anyhow::Result<Subscription> {
        let mut g = self.subs.write();
        let s = g.remove(sid).context("no such subscription")?;
        Ok(s)
    }
}

#[derive(Debug)]
pub enum SubscribeRequest {
    Create {
        callback: url::Url,
        timeout: Duration,
    },
    Renew {
        sid: String,
        timeout: Duration,
    },
}

impl core::fmt::Display for SubscribeRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubscribeRequest::Create { callback, timeout } => {
                write!(f, "create;callback={callback};timeout={timeout:?}")
            }
            SubscribeRequest::Renew { sid, timeout } => {
                write!(f, "renew;sid={sid};timeout={timeout:?}")
            }
        }
    }
}

impl SubscribeRequest {
    fn timeout(&self) -> Duration {
        match self {
            SubscribeRequest::Create { timeout, .. } => *timeout,
            SubscribeRequest::Renew { timeout, .. } => *timeout,
        }
    }
}

impl SubscribeRequest {
    #[allow(clippy::result_large_err)]
    pub fn parse(
        request: axum::extract::Request,
    ) -> Result<SubscribeRequest, axum::response::Response> {
        if request.method().as_str() != "SUBSCRIBE" {
            return Err(StatusCode::METHOD_NOT_ALLOWED.into_response());
        }

        let (parts, _body) = request.into_parts();
        let is_event = parts
            .headers
            .get(HeaderName::from_static("nt"))
            .map(|v| v.as_bytes() == b"upnp:event")
            .unwrap_or_default();

        let callback = parts
            .headers
            .get(HeaderName::from_static("callback"))
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim_matches(|c| c == '>' || c == '<'))
            .and_then(|u| url::Url::parse(u).ok());
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

        let timeout = timeout.unwrap_or(DEFAULT_TIMEOUT).min(DEFAULT_TIMEOUT);

        match (is_event, callback, subscription_id) {
            (true, Some(callback), None) => Ok(SubscribeRequest::Create { callback, timeout }),
            (_, _, Some(sid)) => Ok(SubscribeRequest::Renew {
                sid: sid.to_owned(),
                timeout,
            }),
            _ => Err(StatusCode::BAD_REQUEST.into_response()),
        }
    }
}

#[derive(Debug)]
pub(crate) enum SubscriptionResult {
    Renewed { sid: String },
    Created { sid: String },
}

impl SubscriptionResult {
    fn sid(&self) -> &str {
        match self {
            SubscriptionResult::Renewed { sid } => sid,
            SubscriptionResult::Created { sid } => sid,
        }
    }
}

pub(crate) fn subscription_into_response(
    request: &SubscribeRequest,
    result: anyhow::Result<SubscriptionResult>,
) -> axum::response::Response {
    trace!(%request, ?result, "request->response");

    let result = match result {
        Ok(r) => r,
        Err(e) => {
            warn!(error=?e, sub=?request, "error handling subscription request");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    (
        StatusCode::OK,
        [
            ("SID", result.sid().to_owned()),
            ("TIMEOUT", format!("Second-{}", request.timeout().as_secs())),
        ],
    )
        .into_response()
}

impl UpnpServerStateInner {
    pub(crate) fn handle_content_directory_subscription_request(
        self: &Arc<Self>,
        req: &SubscribeRequest,
    ) -> anyhow::Result<SubscriptionResult> {
        match req {
            SubscribeRequest::Create { callback, timeout } => {
                let sid = self.new_content_directory_subscription(callback.clone(), *timeout)?;
                Ok(SubscriptionResult::Created { sid })
            }
            SubscribeRequest::Renew { sid, timeout } => {
                self.content_directory_subscriptions
                    .update_timeout(sid, *timeout)?;
                Ok(SubscriptionResult::Renewed { sid: sid.clone() })
            }
        }
    }

    pub(crate) fn handle_connection_manager_subscription_request(
        self: &Arc<Self>,
        req: &SubscribeRequest,
    ) -> anyhow::Result<SubscriptionResult> {
        match req {
            SubscribeRequest::Create { callback, timeout } => {
                let sid = self.new_connection_manager_subscription(callback.clone(), *timeout)?;
                Ok(SubscriptionResult::Created { sid })
            }
            SubscribeRequest::Renew { sid, timeout } => {
                self.connection_manager_subscriptions
                    .update_timeout(sid, *timeout)?;
                Ok(SubscriptionResult::Renewed { sid: sid.clone() })
            }
        }
    }

    fn new_content_directory_subscription(
        self: &Arc<Self>,
        url: url::Url,
        timeout: Duration,
    ) -> anyhow::Result<String> {
        let (sid, refresh_notify) = self
            .content_directory_subscriptions
            .add(url.clone(), timeout);
        let token = self.cancel_token.child_token();

        // Spawn a task that will notify it of system id changes.
        // Spawn a task that will wait for timeout or subscription refreshes.
        // When it times out, kill all of them.

        let pspan = self.span.clone();
        let subscription_manager = {
            let mut brx = self.system_update_bcast_tx.subscribe();
            let state = Arc::downgrade(self);
            let sid = sid.clone();
            let url = url.clone();

            async move {
                use crate::services::content_directory::subscription::notify_system_id_update;
                let system_update_id_notifier = async {
                    loop {
                        let res = brx.recv().await;
                        let state = state.upgrade().context("upnp server dead")?;
                        let seq = state.content_directory_subscriptions.next_seq(&sid)?;
                        match res {
                            Ok(system_update_id) => {
                                trace!(system_update_id, "notifying SystemUpdateId update");
                                if let Err(e) =
                                    notify_system_id_update(&url, &sid, seq, system_update_id).await
                                {
                                    debug!(error=?e, "error updating UPNP subscription");
                                }
                            }
                            Err(RecvError::Lagged(by)) => {
                                warn!(by, "UPNP subscription lagged");
                                let seq = state.content_directory_subscriptions.next_seq(&sid)?;
                                let system_update_id =
                                    state.system_update_id.load(Ordering::Relaxed);
                                if let Err(e) =
                                    notify_system_id_update(&url, &sid, seq, system_update_id).await
                                {
                                    debug!(error=?e, "error updating UPNP subscription");
                                }
                            }
                            Err(RecvError::Closed) => return Ok(()),
                        }
                    }
                }
                .instrument(error_span!("system-update-id-notifier"));

                let timeout_notifier = async {
                    let mut timeout = timeout;
                    loop {
                        tokio::select! {
                            _ = refresh_notify.notified() => {
                                timeout = state.upgrade().context("upnp server dead")?.content_directory_subscriptions.get_timeout(&sid)?;
                                trace!(?timeout, "refreshed subscription");
                            },
                            _ = tokio::time::sleep(timeout) => {
                                state.upgrade().context("upnp server dead")?.content_directory_subscriptions.remove(&sid)?;
                                trace!(?timeout, "subscription timed out, removing");
                                return Ok(())
                            }
                        }
                    }
                }.instrument(error_span!("timeout-killer"));

                tokio::select! {
                    r = system_update_id_notifier => r,
                    r = timeout_notifier => r,
                }
            }
        };

        spawn_with_cancel(
            error_span!(parent: pspan, "subscription-manager", sid, %url, service="ContentDirectory"),
            token,
            subscription_manager,
        );

        Ok(sid)
    }

    fn new_connection_manager_subscription(
        self: &Arc<Self>,
        url: url::Url,
        timeout: Duration,
    ) -> anyhow::Result<String> {
        let (sid, refresh_notify) = self
            .connection_manager_subscriptions
            .add(url.clone(), timeout);
        let token = self.cancel_token.clone();

        // Spawn a task that will notify it of system id changes.
        // Spawn a task that will wait for timeout or subscription refreshes.
        // When it times out, kill all of them.

        let pspan = self.span.clone();
        let subscription_manager = {
            let state = Arc::downgrade(self);
            let sid = sid.clone();

            async move {
                let timeout_notifier = async {
                    let mut timeout = timeout;
                    loop {
                        tokio::select! {
                            _ = refresh_notify.notified() => {
                                timeout = state.upgrade().context("upnp server dead")?.connection_manager_subscriptions.get_timeout(&sid)?;
                            },
                            _ = tokio::time::sleep(timeout) => {
                                state.upgrade().context("upnp server dead")?.connection_manager_subscriptions.remove(&sid)?;
                                return Ok(())
                            }
                        }
                    }
                }.instrument(error_span!("timeout-killer"));

                timeout_notifier.await
            }
        };

        spawn_with_cancel(
            error_span!(parent: pspan, "subscription-manager", sid, %url, service="ConnectionManager"),
            token,
            subscription_manager,
        );

        Ok(sid)
    }
}
