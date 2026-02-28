use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use parking_lot::RwLock;
use serde_derive::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};
use url::Url;

const SSDP_SEARCH_MEDIA_RENDERER: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
const SERVICE_AV_TRANSPORT: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const SERVICE_RENDERING_CONTROL: &str = "urn:schemas-upnp-org:service:RenderingControl:1";

/// A discovered UPnP media renderer.
#[derive(Debug, Clone, Serialize)]
pub struct MediaRenderer {
    /// Stable identifier derived from the device's UPnP location URL.
    pub id: String,
    pub friendly_name: String,
    pub location: Url,
    av_transport_control_url: Option<Url>,
    rendering_control_url: Option<Url>,
}

/// Current transport state of a renderer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RendererStatus {
    pub transport_state: String,
    pub current_uri: String,
    pub current_position: String,
    pub duration: String,
    pub volume: Option<u32>,
}

/// Options for the control point.
pub struct ControlPointOptions {
    pub discover_interval: Duration,
    pub discover_timeout: Duration,
    /// Renderers not seen within this duration are removed from the list.
    pub renderer_expiry: Duration,
    pub cancellation_token: CancellationToken,
}

impl Default for ControlPointOptions {
    fn default() -> Self {
        Self {
            discover_interval: Duration::from_secs(30),
            discover_timeout: Duration::from_secs(5),
            renderer_expiry: Duration::from_secs(90),
            cancellation_token: CancellationToken::new(),
        }
    }
}

struct RendererEntry {
    renderer: MediaRenderer,
    last_seen: Instant,
}

/// The UPnP control point. Discovers media renderers and sends commands.
pub struct UpnpControlPoint {
    renderers: Arc<RwLock<HashMap<String, RendererEntry>>>,
    client: reqwest::Client,
    opts: ControlPointOptions,
}

impl UpnpControlPoint {
    pub fn new(opts: ControlPointOptions) -> Self {
        Self {
            renderers: Arc::new(RwLock::new(HashMap::new())),
            client: reqwest::Client::new(),
            opts,
        }
    }

    /// List all currently discovered (non-expired) renderers.
    pub fn list_renderers(&self) -> Vec<MediaRenderer> {
        let now = Instant::now();
        let expiry = self.opts.renderer_expiry;
        self.renderers
            .read()
            .values()
            .filter(|e| now.duration_since(e.last_seen) < expiry)
            .map(|e| e.renderer.clone())
            .collect()
    }

    /// Get a renderer by ID (only if not expired).
    pub fn get_renderer(&self, id: &str) -> Option<MediaRenderer> {
        let now = Instant::now();
        let expiry = self.opts.renderer_expiry;
        self.renderers.read().get(id).and_then(|e| {
            if now.duration_since(e.last_seen) < expiry {
                Some(e.renderer.clone())
            } else {
                None
            }
        })
    }

    /// Set the media URI on the renderer and start playing.
    pub async fn play(&self, renderer_id: &str, uri: Option<&str>) -> anyhow::Result<()> {
        let renderer = self.require_renderer(renderer_id)?;
        let control_url = renderer.av_transport_url()?;

        if let Some(uri) = uri {
            let escaped_uri = quick_xml::escape::escape(uri);
            self.soap_action(
                control_url,
                SERVICE_AV_TRANSPORT,
                "SetAVTransportURI",
                &format!(
                    "<InstanceID>0</InstanceID>\
                     <CurrentURI>{escaped_uri}</CurrentURI>\
                     <CurrentURIMetaData></CurrentURIMetaData>"
                ),
            )
            .await
            .context("SetAVTransportURI failed")?;
        }

        self.soap_action(
            control_url,
            SERVICE_AV_TRANSPORT,
            "Play",
            "<InstanceID>0</InstanceID><Speed>1</Speed>",
        )
        .await
        .context("Play failed")
    }

    /// Pause playback.
    pub async fn pause(&self, renderer_id: &str) -> anyhow::Result<()> {
        let renderer = self.require_renderer(renderer_id)?;
        self.soap_action(
            renderer.av_transport_url()?,
            SERVICE_AV_TRANSPORT,
            "Pause",
            "<InstanceID>0</InstanceID>",
        )
        .await
    }

    /// Stop playback.
    pub async fn stop(&self, renderer_id: &str) -> anyhow::Result<()> {
        let renderer = self.require_renderer(renderer_id)?;
        self.soap_action(
            renderer.av_transport_url()?,
            SERVICE_AV_TRANSPORT,
            "Stop",
            "<InstanceID>0</InstanceID>",
        )
        .await
    }

    /// Seek to a position (format: "HH:MM:SS").
    pub async fn seek(&self, renderer_id: &str, position: &str) -> anyhow::Result<()> {
        let renderer = self.require_renderer(renderer_id)?;
        self.soap_action(
            renderer.av_transport_url()?,
            SERVICE_AV_TRANSPORT,
            "Seek",
            &format!(
                "<InstanceID>0</InstanceID>\
                 <Unit>REL_TIME</Unit>\
                 <Target>{position}</Target>"
            ),
        )
        .await
    }

    /// Set volume (0-100).
    pub async fn set_volume(&self, renderer_id: &str, volume: u32) -> anyhow::Result<()> {
        let renderer = self.require_renderer(renderer_id)?;
        let control_url = renderer
            .rendering_control_url
            .as_ref()
            .context("renderer has no RenderingControl service")?;
        self.soap_action(
            control_url,
            SERVICE_RENDERING_CONTROL,
            "SetVolume",
            &format!(
                "<InstanceID>0</InstanceID>\
                 <Channel>Master</Channel>\
                 <DesiredVolume>{volume}</DesiredVolume>"
            ),
        )
        .await
    }

    /// Get current transport status and volume.
    pub async fn get_status(&self, renderer_id: &str) -> anyhow::Result<RendererStatus> {
        let renderer = self.require_renderer(renderer_id)?;
        let control_url = renderer.av_transport_url()?;

        let transport_info = self
            .soap_action_response(
                control_url,
                SERVICE_AV_TRANSPORT,
                "GetTransportInfo",
                "<InstanceID>0</InstanceID>",
            )
            .await
            .context("GetTransportInfo failed")?;

        let position_info = self
            .soap_action_response(
                control_url,
                SERVICE_AV_TRANSPORT,
                "GetPositionInfo",
                "<InstanceID>0</InstanceID>",
            )
            .await
            .context("GetPositionInfo failed")?;

        let volume = if let Some(rc_url) = &renderer.rendering_control_url {
            let vol_resp = self
                .soap_action_response(
                    rc_url,
                    SERVICE_RENDERING_CONTROL,
                    "GetVolume",
                    "<InstanceID>0</InstanceID><Channel>Master</Channel>",
                )
                .await;
            match vol_resp {
                Ok(resp) => extract_xml_value(&resp, "CurrentVolume").and_then(|v| v.parse().ok()),
                Err(e) => {
                    debug!("GetVolume failed: {e:#}");
                    None
                }
            }
        } else {
            None
        };

        Ok(RendererStatus {
            transport_state: extract_xml_value(&transport_info, "CurrentTransportState")
                .unwrap_or_default(),
            current_uri: extract_xml_value(&position_info, "TrackURI").unwrap_or_default(),
            current_position: extract_xml_value(&position_info, "RelTime").unwrap_or_default(),
            duration: extract_xml_value(&position_info, "TrackDuration").unwrap_or_default(),
            volume,
        })
    }

    /// Run the discovery loop. Spawned as a background task that periodically discovers renderers.
    pub async fn run_discovery(&self) -> anyhow::Result<()> {
        let mut interval = tokio::time::interval(self.opts.discover_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = self.opts.cancellation_token.cancelled() => {
                    debug!("control point discovery cancelled");
                    return Ok(());
                }
                _ = interval.tick() => {
                    if let Err(e) = self.discover_renderers().await {
                        warn!("renderer discovery failed: {e:#}");
                    }
                    // Purge expired entries while we hold the lock.
                    let mut renderers = self.renderers.write();
                    let expiry = self.opts.renderer_expiry;
                    let now = Instant::now();
                    renderers.retain(|_, e| now.duration_since(e.last_seen) < expiry);
                }
            }
        }
    }

    async fn discover_renderers(&self) -> anyhow::Result<()> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        librqbit_upnp::discover_once(
            &tx,
            SSDP_SEARCH_MEDIA_RENDERER,
            self.opts.discover_timeout,
            None,
        )
        .await
        .context("SSDP discovery failed")?;

        drop(tx);

        while let Some(response) = rx.recv().await {
            let location = response.location.clone();
            match self.process_discovered_device(response).await {
                Ok(Some(name)) => debug!(name, %location, "discovered media renderer"),
                Ok(None) => trace!(%location, "device is not a media renderer"),
                Err(e) => debug!(%location, "failed to process discovered device: {e:#}"),
            }
        }

        Ok(())
    }

    async fn process_discovered_device(
        &self,
        response: librqbit_upnp::UpnpDiscoverResponse,
    ) -> anyhow::Result<Option<String>> {
        let root_desc = librqbit_upnp::discover_services(response.location.clone()).await?;

        for device in &root_desc.devices {
            if let Some(renderer) = Self::extract_renderer(device, &response.location) {
                let name = renderer.friendly_name.clone();
                let mut renderers = self.renderers.write();
                renderers.insert(
                    renderer.id.clone(),
                    RendererEntry {
                        renderer,
                        last_seen: Instant::now(),
                    },
                );

                return Ok(Some(name));
            }
        }

        Ok(None)
    }

    fn extract_renderer(device: &librqbit_upnp::Device, base_url: &Url) -> Option<MediaRenderer> {
        // Check this device and all sub-devices
        if device.device_type.contains("MediaRenderer") {
            return Self::build_renderer(device, base_url);
        }
        for sub in &device.device_list.devices {
            if let Some(r) = Self::extract_renderer(sub, base_url) {
                return Some(r);
            }
        }
        None
    }

    fn build_renderer(device: &librqbit_upnp::Device, base_url: &Url) -> Option<MediaRenderer> {
        let id = renderer_id_from_url(base_url);

        let mut av_transport_url = None;
        let mut rendering_control_url = None;

        for service in &device.service_list.services {
            if service.service_type.contains("AVTransport") {
                av_transport_url = base_url.join(&service.control_url).ok();
            } else if service.service_type.contains("RenderingControl") {
                rendering_control_url = base_url.join(&service.control_url).ok();
            }
        }

        Some(MediaRenderer {
            id,
            friendly_name: device.name().to_string(),
            location: base_url.clone(),
            av_transport_control_url: av_transport_url,
            rendering_control_url,
        })
    }

    fn require_renderer(&self, id: &str) -> anyhow::Result<MediaRenderer> {
        self.get_renderer(id)
            .context(format!("renderer '{id}' not found"))
    }

    async fn soap_action(
        &self,
        control_url: &Url,
        service_type: &str,
        action: &str,
        args: &str,
    ) -> anyhow::Result<()> {
        self.soap_action_response(control_url, service_type, action, args)
            .await
            .map(|_| ())
    }

    async fn soap_action_response(
        &self,
        control_url: &Url,
        service_type: &str,
        action: &str,
        args: &str,
    ) -> anyhow::Result<String> {
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"
    s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
    <s:Body>
        <u:{action} xmlns:u="{service_type}">
            {args}
        </u:{action}>
    </s:Body>
</s:Envelope>"#
        );

        let response = self
            .client
            .post(control_url.clone())
            .header("Content-Type", "text/xml; charset=\"utf-8\"")
            .header("SOAPAction", format!("\"{service_type}#{action}\""))
            .body(body)
            .send()
            .await
            .with_context(|| format!("failed to send {action} to {control_url}"))?;

        let status = response.status();
        let text = response
            .text()
            .await
            .context("failed to read SOAP response")?;

        trace!(action, %status, response = %text, "SOAP response");

        if !status.is_success() {
            let upnp_error = extract_xml_value(&text, "errorDescription")
                .unwrap_or_else(|| format!("HTTP {status}"));
            let error_code = extract_xml_value(&text, "errorCode").unwrap_or_default();
            debug!(action, %status, %control_url, error_code, upnp_error, "SOAP action failed");
            bail!("{action} failed: {upnp_error} (UPnP error {error_code})");
        }

        Ok(text)
    }
}

impl MediaRenderer {
    fn av_transport_url(&self) -> anyhow::Result<&Url> {
        self.av_transport_control_url
            .as_ref()
            .context("renderer has no AVTransport service")
    }
}

fn renderer_id_from_url(url: &Url) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    url.as_str().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Simple XML value extractor — finds `<tag>value</tag>` in a SOAP response.
fn extract_xml_value(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_xml_value() {
        let xml = r#"<CurrentVolume>42</CurrentVolume>"#;
        assert_eq!(extract_xml_value(xml, "CurrentVolume"), Some("42".into()));
    }

    #[test]
    fn test_extract_xml_value_nested() {
        let xml = r#"<s:Body><u:GetVolumeResponse><CurrentVolume>75</CurrentVolume></u:GetVolumeResponse></s:Body>"#;
        assert_eq!(extract_xml_value(xml, "CurrentVolume"), Some("75".into()));
    }

    #[test]
    fn test_renderer_id_stable() {
        let url = Url::parse("http://192.168.1.100:8080/description.xml").unwrap();
        let id1 = renderer_id_from_url(&url);
        let id2 = renderer_id_from_url(&url);
        assert_eq!(id1, id2);
    }
}
