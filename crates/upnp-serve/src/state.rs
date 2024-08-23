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
use tracing::{error_span, Span};

use crate::{
    subscriptions::Subscriptions, upnp_types::content_directory::ContentDirectoryBrowseProvider,
};

pub struct UpnpServerStateInner {
    pub rendered_root_description: Bytes,
    pub provider: Box<dyn ContentDirectoryBrowseProvider>,
    pub system_update_id: AtomicU64,
    pub subscriptions: Subscriptions,

    pub span: Span,
    pub system_update_bcast_tx: tokio::sync::broadcast::Sender<u64>,
    pub cancel_token: tokio_util::sync::CancellationToken,
    _drop_guard: tokio_util::sync::DropGuard,
}

fn new_system_update_id() -> anyhow::Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

impl UpnpServerStateInner {
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
            system_update_bcast_tx: btx,
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
                        let _ = state.system_update_bcast_tx.send(new_value);
                    }
                }
            },
        );

        Ok(state)
    }
}

pub type UnpnServerState = Arc<UpnpServerStateInner>;
