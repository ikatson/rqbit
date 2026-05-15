use std::{
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use axum::body::Bytes;
use dashmap::DashMap;
use librqbit_core::spawn_utils::spawn_with_cancel;
use tokio_util::sync::CancellationToken;
use tracing::{Span, debug_span};

use crate::{ContentDirectoryBrowseProvider, subscriptions::Subscriptions};

/// Capabilities discovered from a UPnP renderer (TV) via GetProtocolInfo.
#[derive(Clone, Debug)]
pub struct RendererCapabilities {
    pub supports_dts: bool,
}

pub struct UpnpServerStateInner {
    pub(crate) rendered_root_description: Bytes,
    pub(crate) provider: Box<dyn ContentDirectoryBrowseProvider>,
    pub(crate) system_update_id: AtomicU64,
    pub(crate) content_directory_subscriptions: Subscriptions,
    pub(crate) connection_manager_subscriptions: Subscriptions,

    pub(crate) span: Span,
    pub(crate) system_update_bcast_tx: tokio::sync::broadcast::Sender<u64>,
    pub(crate) cancel_token: tokio_util::sync::CancellationToken,
    _drop_guard: tokio_util::sync::DropGuard,

    /// Per-IP renderer capabilities, populated by background SSDP discovery.
    #[allow(dead_code)]
    pub(crate) renderer_capabilities: Arc<DashMap<IpAddr, RendererCapabilities>>,
    /// Optional extractor for client IP from request extensions (set by the embedder).
    pub(crate) client_ip_extractor:
        Option<Arc<dyn Fn(&http::Extensions) -> Option<IpAddr> + Send + Sync>>,
}

fn new_system_update_id() -> anyhow::Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

impl UpnpServerStateInner {
    pub fn new(
        rendered_root_description: Bytes,
        provider: Box<dyn ContentDirectoryBrowseProvider>,
        cancellation_token: CancellationToken,
        renderer_capabilities: Arc<DashMap<IpAddr, RendererCapabilities>>,
        client_ip_extractor: Option<Arc<dyn Fn(&http::Extensions) -> Option<IpAddr> + Send + Sync>>,
    ) -> anyhow::Result<Arc<Self>> {
        let cancel_token = cancellation_token.child_token();
        let drop_guard = cancel_token.clone().drop_guard();
        let (btx, _) = tokio::sync::broadcast::channel(32);
        let span = debug_span!(parent: None, "upnp-server");
        let state = Arc::new(Self {
            rendered_root_description,
            provider,
            system_update_id: AtomicU64::new(new_system_update_id()?),
            content_directory_subscriptions: Default::default(),
            connection_manager_subscriptions: Default::default(),
            system_update_bcast_tx: btx,
            _drop_guard: drop_guard,
            span: span.clone(),
            cancel_token: cancel_token.clone(),
            renderer_capabilities,
            client_ip_extractor,
        });

        spawn_with_cancel::<anyhow::Error>(
            debug_span!(parent: span, "system_update_id_updater"),
            "system_update_id_updater",
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
