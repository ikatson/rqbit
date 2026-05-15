use std::{net::IpAddr, sync::Arc};

use dashmap::DashMap;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::state::RendererCapabilities;

/// Parse the ConnectionManager control URL from a UPnP device description XML.
/// Returns the absolute URL resolved against the description's base URL.
fn parse_connection_manager_url(xml: &str, base_url: &str) -> Option<String> {
    // We look for the ConnectionManager service block and extract its controlURL.
    // Simple approach: find the serviceType, then find the next controlURL in that block.
    let cm_marker = "ConnectionManager";
    let cm_pos = xml.find(cm_marker)?;
    let after_cm = &xml[cm_pos..];

    let ctrl_start = after_cm.find("<controlURL>")?;
    let rest = &after_cm[ctrl_start + "<controlURL>".len()..];
    let ctrl_end = rest.find("</controlURL>")?;
    let control_path = rest[..ctrl_end].trim();

    // Resolve relative path against base URL.
    let base = url::Url::parse(base_url).ok()?;
    let resolved = base.join(control_path).ok()?;
    Some(resolved.to_string())
}

/// Check whether a GetProtocolInfo Sink string indicates DTS support.
fn sink_supports_dts(sink: &str) -> bool {
    let lower = sink.to_lowercase();
    lower.contains("audio/vnd.dts")
        || lower.contains("audio/x-dts")
        || lower.contains("dlna.org_pn=dts")
}

async fn probe_renderer(
    ip: IpAddr,
    location: String,
    capabilities: Arc<DashMap<IpAddr, RendererCapabilities>>,
    client: reqwest::Client,
) {
    debug!(%ip, %location, "probing renderer capabilities");

    let result: anyhow::Result<()> = async {
        // 1. Fetch device description XML.
        let desc_xml = client
            .get(&location)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await?
            .text()
            .await?;

        // 2. Find ConnectionManager control URL.
        let Some(cm_url) = parse_connection_manager_url(&desc_xml, &location) else {
            debug!(%ip, "no ConnectionManager found in device description");
            return Ok(());
        };

        // 3. SOAP: GetProtocolInfo
        let soap = r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"
            s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:GetProtocolInfo xmlns:u="urn:schemas-upnp-org:service:ConnectionManager:1"/>
  </s:Body>
</s:Envelope>"#;

        let resp = client
            .post(&cm_url)
            .header(
                "SOAPACTION",
                "\"urn:schemas-upnp-org:service:ConnectionManager:1#GetProtocolInfo\"",
            )
            .header("Content-Type", "text/xml; charset=\"utf-8\"")
            .timeout(std::time::Duration::from_secs(5))
            .body(soap)
            .send()
            .await?
            .text()
            .await?;

        // 4. Extract <Sink> value and detect DTS.
        let supports_dts = if let Some(start) = resp.find("<Sink>") {
            let after = &resp[start + "<Sink>".len()..];
            let end = after.find("</Sink>").unwrap_or(after.len());
            sink_supports_dts(&after[..end])
        } else {
            false
        };

        debug!(%ip, supports_dts, "renderer capability probed");
        capabilities.insert(ip, RendererCapabilities { supports_dts });
        Ok(())
    }
    .await;

    if let Err(e) = result {
        warn!(%ip, error=?e, "failed to probe renderer capabilities");
    }
}

/// Background task: receives (IpAddr, location_url) from the SSDP listener,
/// probes each renderer's GetProtocolInfo once, and stores results.
pub async fn run_renderer_discovery(
    mut rx: mpsc::Receiver<(IpAddr, String)>,
    capabilities: Arc<DashMap<IpAddr, RendererCapabilities>>,
) {
    let client = reqwest::Client::new();
    while let Some((ip, location)) = rx.recv().await {
        if capabilities.contains_key(&ip) {
            continue; // already known
        }
        tokio::spawn(probe_renderer(
            ip,
            location,
            capabilities.clone(),
            client.clone(),
        ));
    }
}
