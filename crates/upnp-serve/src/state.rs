use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use axum::body::Bytes;
use librqbit_core::spawn_utils::spawn_with_cancel;
use tokio::sync::broadcast::error::RecvError;
use tracing::{error_span, warn, Instrument, Span};

use crate::{
    subscriptions::{notify_subscription_system_update, Subscriptions},
    upnp_types::content_directory::ContentDirectoryBrowseProvider,
};

pub struct UnpnServerStateInner {
    pub rendered_root_description: Bytes,
    pub provider: Box<dyn ContentDirectoryBrowseProvider>,
    pub system_update_id: AtomicU64,
    pub subscriptions: Subscriptions,

    span: Span,
    system_update_bcast_tx: tokio::sync::broadcast::Sender<u64>,
    cancel_token: tokio_util::sync::CancellationToken,
    _drop_guard: tokio_util::sync::DropGuard,
}

fn new_system_update_id() -> anyhow::Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

impl UnpnServerStateInner {
    pub fn new(
        rendered_root_description: Bytes,
        provider: Box<dyn ContentDirectoryBrowseProvider>,
    ) -> anyhow::Result<Arc<Self>> {
        let cancel_token = tokio_util::sync::CancellationToken::new();
        let drop_guard = cancel_token.clone().drop_guard();
        let (btx, _) = tokio::sync::broadcast::channel(32);
        let span = error_span!(parent: None, "upnp-server");
        let state = Arc::new(Self {
            rendered_root_description,
            provider,
            system_update_id: AtomicU64::new(new_system_update_id()?),
            subscriptions: Default::default(),
            system_update_bcast_tx: btx.clone(),
            _drop_guard: drop_guard,
            span: span.clone(),
            cancel_token: cancel_token.clone(),
        });

        spawn_with_cancel(
            error_span!(parent: span, "system_update_id_updater"),
            cancel_token,
            {
                let state = Arc::downgrade(&state);
                async move {
                    let mut interval = tokio::time::interval(Duration::from_secs(10));
                    loop {
                        interval.tick().await;
                        let new_value = new_system_update_id()?;
                        let state = state.upgrade().context("upnp server is dead")?;
                        state.system_update_id.store(new_value, Ordering::Relaxed);
                        state.system_update_bcast_tx.send(new_value);
                    }
                }
            },
        );

        Ok(state)
    }

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

        let state = self.clone();
        let pspan = self.span.clone();
        let subscription_manager = {
            let mut brx = state.system_update_bcast_tx.subscribe();
            let state = Arc::downgrade(&state);
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
                                    warn!(error=?e, "error updating UPNP subscription");
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
                                    warn!(error=?e, "error updating UPNP subscription");
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
            error_span!(parent: pspan, "subscription-manager", ?url),
            token,
            subscription_manager,
        );

        Ok(sid)
    }
}

pub type UnpnServerState = Arc<UnpnServerStateInner>;
