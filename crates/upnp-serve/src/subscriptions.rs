use anyhow::Context;
use http::Method;
use parking_lot::RwLock;
use reqwest::RequestBuilder;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::Notify;

use crate::templates::render_notify_subscription_system_update_id;

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
