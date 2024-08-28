use crate::state::UpnpServerStateInner;
use anyhow::Context;
use axum::response::IntoResponse;
use http::{HeaderName, StatusCode};
use librqbit_core::spawn_utils::spawn_with_cancel;
use parking_lot::RwLock;
use std::{
    collections::HashMap,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tokio::sync::{broadcast::error::RecvError, Notify};
use tracing::{debug, error_span, trace, warn, Instrument};

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

impl UpnpServerStateInner {
    pub fn renew_content_directory_subscription(
        &self,
        sid: &str,
        new_timeout: Duration,
    ) -> anyhow::Result<()> {
        self.content_directory_subscriptions
            .update_timeout(sid, new_timeout)
    }

    pub fn new_content_directory_subscription(
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
                            },
                            _ = tokio::time::sleep(timeout) => {
                                state.upgrade().context("upnp server dead")?.content_directory_subscriptions.remove(&sid)?;
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
            error_span!(parent: pspan, "subscription-manager", sid, %url),
            token,
            subscription_manager,
        );

        Ok(sid)
    }
}

pub struct SubscribeRequest {
    pub callback: url::Url,
    pub subscription_id: Option<String>,
    pub timeout: Duration,
}

impl SubscribeRequest {
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
        if !is_event {
            return Err((StatusCode::BAD_REQUEST, "expected NT: upnp:event header").into_response());
        }

        let callback = parts
            .headers
            .get(HeaderName::from_static("callback"))
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim_matches(|c| c == '>' || c == '<'))
            .and_then(|u| url::Url::parse(u).ok());
        let callback = match callback {
            Some(c) => c,
            None => return Err((StatusCode::BAD_REQUEST, "callback not provided").into_response()),
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

        Ok(SubscribeRequest {
            callback,
            subscription_id: subscription_id.map(|s| s.to_owned()),
            timeout,
        })
    }
}
