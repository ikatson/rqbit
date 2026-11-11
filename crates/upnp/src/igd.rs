//! The SOAP actions of the UPnP IGD `WANIPConnection` / `WANPPPConnection` services that the
//! mapper uses: `AddPortMapping`, `GetExternalIPAddress` and `DeletePortMapping`.

use std::{fmt, net::IpAddr};

use quick_xml::{Reader, events::Event};
use reqwest::Client;
use tracing::trace;
use url::Url;

use crate::PortMappingProtocol;

pub(crate) const SERVICE_TYPE_WAN_IP_CONNECTION_1: &str =
    "urn:schemas-upnp-org:service:WANIPConnection:1";
pub(crate) const SERVICE_TYPE_WAN_IP_CONNECTION_2: &str =
    "urn:schemas-upnp-org:service:WANIPConnection:2";
pub(crate) const SERVICE_TYPE_WAN_PPP_CONNECTION_1: &str =
    "urn:schemas-upnp-org:service:WANPPPConnection:1";

/// The services that can map ports. Version 2 of `WANIPConnection` keeps the version 1 actions
/// and arguments we use, so the same requests work on all three.
pub(crate) const WAN_SERVICE_TYPES: [&str; 3] = [
    SERVICE_TYPE_WAN_IP_CONNECTION_1,
    SERVICE_TYPE_WAN_IP_CONNECTION_2,
    SERVICE_TYPE_WAN_PPP_CONNECTION_1,
];

/// `ConflictInMappingEntry`: the external port is already mapped to another host.
pub(crate) const ERROR_CONFLICT_IN_MAPPING_ENTRY: u16 = 718;
/// `OnlyPermanentLeasesSupported`: the router wants a lease duration of 0.
pub(crate) const ERROR_ONLY_PERMANENT_LEASES_SUPPORTED: u16 = 725;

#[derive(Debug)]
pub(crate) enum SoapError {
    /// The router answered with a UPnP error (a SOAP fault with an `errorCode`).
    Upnp { code: u16, description: String },
    /// The request did not get a usable answer (connection, HTTP status, unreadable body).
    Transport(anyhow::Error),
}

impl fmt::Display for SoapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SoapError::Upnp { code, description } => {
                write!(f, "UPnP error {code}")?;
                if !description.is_empty() {
                    write!(f, " {description}")?;
                }
                Ok(())
            }
            SoapError::Transport(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for SoapError {}

fn protocol_name(protocol: PortMappingProtocol) -> &'static str {
    match protocol {
        PortMappingProtocol::Tcp => "TCP",
        PortMappingProtocol::Udp => "UDP",
    }
}

/// Builds the SOAP envelope of a UPnP action. Argument values are escaped; names are ours.
pub(crate) fn soap_envelope(service_type: &str, action: &str, args: &[(&str, &str)]) -> String {
    use std::fmt::Write;
    let mut body = String::new();
    for (name, value) in args {
        // Writing to a String does not fail.
        let _ = write!(
            body,
            "<{name}>{}</{name}>",
            quick_xml::escape::escape(*value)
        );
    }
    format!(
        "<?xml version=\"1.0\"?>\r\n\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><u:{action} xmlns:u=\"{service_type}\">{body}</u:{action}></s:Body>\
         </s:Envelope>\r\n"
    )
}

/// Returns the unescaped text of the first element called `name` (any namespace prefix), or
/// `None` when there is none or the XML is broken before it.
pub(crate) fn element_text(xml: &str, name: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.local_name().as_ref() == name.as_bytes() => {
                let end = e.to_end().into_owned();
                let text = reader.read_text(end.name()).ok()?;
                let raw = text.decode().ok()?;
                return quick_xml::escape::unescape(&raw)
                    .ok()
                    .map(|s| s.trim().to_owned());
            }
            Ok(Event::Empty(e)) if e.local_name().as_ref() == name.as_bytes() => {
                return Some(String::new());
            }
            Ok(Event::Eof) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

/// Reads the UPnP error out of a SOAP fault body.
pub(crate) fn parse_upnp_error(body: &str) -> Option<(u16, String)> {
    let code = element_text(body, "errorCode")?.parse().ok()?;
    let description = element_text(body, "errorDescription").unwrap_or_default();
    Some((code, description))
}

/// One `WANIPConnection` / `WANPPPConnection` service of an internet gateway device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IgdService {
    pub control_url: Url,
    pub service_type: String,
}

impl IgdService {
    async fn call(
        &self,
        client: &Client,
        action: &str,
        args: &[(&str, &str)],
    ) -> Result<String, SoapError> {
        let body = soap_envelope(&self.service_type, action, args);
        let response = client
            .post(self.control_url.clone())
            .header("Content-Type", "text/xml; charset=\"utf-8\"")
            .header("SOAPAction", format!("\"{}#{action}\"", self.service_type))
            .body(body)
            .send()
            .await
            .map_err(|e| SoapError::Transport(anyhow::Error::new(e).context("error sending")))?;
        let status = response.status();
        let text = response.text().await.map_err(|e| {
            SoapError::Transport(anyhow::Error::new(e).context("error reading the response"))
        })?;
        trace!(%status, action, text, "SOAP response");
        if status.is_success() {
            return Ok(text);
        }
        match parse_upnp_error(&text) {
            Some((code, description)) => Err(SoapError::Upnp { code, description }),
            None => Err(SoapError::Transport(anyhow::anyhow!(
                "{action} failed with HTTP status {status}"
            ))),
        }
    }

    /// `AddPortMapping` with the external port equal to the internal one. A lease of 0 asks for
    /// a permanent mapping.
    pub async fn add_port_mapping(
        &self,
        client: &Client,
        protocol: PortMappingProtocol,
        port: u16,
        internal_client: IpAddr,
        description: &str,
        lease_secs: u32,
    ) -> Result<(), SoapError> {
        let port = port.to_string();
        let internal_client = internal_client.to_string();
        let lease = lease_secs.to_string();
        self.call(
            client,
            "AddPortMapping",
            &[
                ("NewRemoteHost", ""),
                ("NewExternalPort", &port),
                ("NewProtocol", protocol_name(protocol)),
                ("NewInternalPort", &port),
                ("NewInternalClient", &internal_client),
                ("NewEnabled", "1"),
                ("NewPortMappingDescription", description),
                ("NewLeaseDuration", &lease),
            ],
        )
        .await
        .map(|_| ())
    }

    pub async fn delete_port_mapping(
        &self,
        client: &Client,
        protocol: PortMappingProtocol,
        external_port: u16,
    ) -> Result<(), SoapError> {
        let port = external_port.to_string();
        self.call(
            client,
            "DeletePortMapping",
            &[
                ("NewRemoteHost", ""),
                ("NewExternalPort", &port),
                ("NewProtocol", protocol_name(protocol)),
            ],
        )
        .await
        .map(|_| ())
    }

    /// `GetExternalIPAddress`. Routers without a public address yet answer with an empty one,
    /// which comes back as `None`.
    pub async fn get_external_ip(&self, client: &Client) -> Result<Option<IpAddr>, SoapError> {
        let body = self.call(client, "GetExternalIPAddress", &[]).await?;
        let text = element_text(&body, "NewExternalIPAddress").ok_or_else(|| {
            SoapError::Transport(anyhow::anyhow!(
                "GetExternalIPAddress answer without NewExternalIPAddress"
            ))
        })?;
        if text.is_empty() {
            return Ok(None);
        }
        text.parse().map(Some).map_err(|_| {
            SoapError::Transport(anyhow::anyhow!(
                "GetExternalIPAddress answered a bad address {text:?}"
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_soap_envelope() {
        let body = soap_envelope(
            SERVICE_TYPE_WAN_IP_CONNECTION_2,
            "AddPortMapping",
            &[
                ("NewExternalPort", "4240"),
                ("NewPortMappingDescription", "a<b&c"),
            ],
        );
        assert!(body.contains(
            "<u:AddPortMapping xmlns:u=\"urn:schemas-upnp-org:service:WANIPConnection:2\">\
             <NewExternalPort>4240</NewExternalPort>\
             <NewPortMappingDescription>a&lt;b&amp;c</NewPortMappingDescription>\
             </u:AddPortMapping>"
        ));
        assert_eq!(
            element_text(&body, "NewPortMappingDescription").as_deref(),
            Some("a<b&c")
        );
        assert_eq!(
            element_text(&body, "NewExternalPort").as_deref(),
            Some("4240")
        );
        assert_eq!(element_text(&body, "Missing"), None);
    }

    #[test]
    fn test_parse_upnp_error() {
        let fault = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring>
<detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0">
<errorCode>718</errorCode><errorDescription>ConflictInMappingEntry</errorDescription>
</UPnPError></detail></s:Fault></s:Body></s:Envelope>"#;
        assert_eq!(
            parse_upnp_error(fault),
            Some((718, "ConflictInMappingEntry".to_owned()))
        );
        assert_eq!(parse_upnp_error("<a><errorCode>x</errorCode></a>"), None);
        assert_eq!(parse_upnp_error("not xml at all <<<"), None);
    }

    #[test]
    fn test_external_ip_element() {
        let body = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>
<u:GetExternalIPAddressResponse xmlns:u="urn:schemas-upnp-org:service:WANIPConnection:1">
<NewExternalIPAddress> 203.0.113.7 </NewExternalIPAddress>
</u:GetExternalIPAddressResponse></s:Body></s:Envelope>"#;
        assert_eq!(
            element_text(body, "NewExternalIPAddress").as_deref(),
            Some("203.0.113.7")
        );
        assert_eq!(
            element_text("<r><NewExternalIPAddress/></r>", "NewExternalIPAddress").as_deref(),
            Some("")
        );
    }
}
