use crate::state::UpnpServerStateInner;
use crate::templates::render_notify_subscription_system_update_id;
use anyhow::Context;
use http::Method;
use librqbit_core::spawn_utils::spawn_with_cancel;
use parking_lot::RwLock;
use std::{
    collections::HashMap,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tokio::sync::{broadcast::error::RecvError, Notify};
use tracing::{debug, error_span, warn, Instrument};

pub struct Subscription {
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

pub async fn notify_subscription_system_update(
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
    let body = render_notify_subscription_system_update_id(system_update_id);

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

impl UpnpServerStateInner {
    pub fn renew_subscription(&self, sid: &str, new_timeout: Duration) -> anyhow::Result<()> {
        self.subscriptions.update_timeout(sid, new_timeout)
    }

    pub fn new_subscription(
        self: &Arc<Self>,
        url: url::Url,
        timeout: Duration,
    ) -> anyhow::Result<String> {
        let (sid, refresh_notify) = self.subscriptions.add(url.clone(), timeout);
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
                let system_update_id_notifier = async {
                    loop {
                        let res = brx.recv().await;
                        let state = state.upgrade().context("upnp server dead")?;
                        let seq = state.subscriptions.next_seq(&sid)?;
                        match res {
                            Ok(system_update_id) => {
                                if let Err(e) = notify_subscription_system_update(
                                    &url,
                                    &sid,
                                    seq,
                                    system_update_id,
                                )
                                .await
                                {
                                    debug!(error=?e, "error updating UPNP subscription");
                                }
                            }
                            Err(RecvError::Lagged(by)) => {
                                warn!(by, "UPNP subscription lagged");
                                let seq = state.subscriptions.next_seq(&sid)?;
                                let system_update_id =
                                    state.system_update_id.load(Ordering::Relaxed);
                                if let Err(e) = notify_subscription_system_update(
                                    &url,
                                    &sid,
                                    seq,
                                    system_update_id,
                                )
                                .await
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
                                timeout = state.upgrade().context("upnp server dead")?.subscriptions.get_timeout(&sid)?;
                            },
                            _ = tokio::time::sleep(timeout) => {
                                state.upgrade().context("upnp server dead")?.subscriptions.remove(&sid)?;
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
            error_span!(parent: pspan, "subscription-manager", %url),
            token,
            subscription_manager,
        );

        Ok(sid)
    }
}
