use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    path::{Path, PathBuf},
    time::Duration,
};

use librqbit::{
    dht::PersistentDht,
    limits::LimitsConfig,
    listen::{ListenerMode, ListenerOptions},
};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RqbitDesktopConfigDht {
    pub disable: bool,
    pub disable_persistence: bool,
    pub persistence_filename: PathBuf,
}

impl Default for RqbitDesktopConfigDht {
    fn default() -> Self {
        Self {
            disable: false,
            disable_persistence: false,
            persistence_filename: PersistentDht::default_persistence_filename().unwrap(),
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RqbitDesktopConfigListen {
    pub enable_tcp: bool,
    pub enable_utp: bool,
    pub enable_upnp_port_forward: bool,
    pub port: u16,
}

impl RqbitDesktopConfigListen {
    pub fn as_listener_opts(&self) -> Option<ListenerOptions> {
        let mode = match (self.enable_tcp, self.enable_utp) {
            (true, true) => ListenerMode::TcpAndUtp,
            (true, false) => ListenerMode::TcpOnly,
            (false, true) => ListenerMode::UtpOnly,
            (false, false) => return None,
        };
        Some(ListenerOptions {
            mode,
            listen_addr: (Ipv4Addr::UNSPECIFIED, self.port).into(),
            enable_upnp_port_forwarding: self.enable_upnp_port_forward,
            ..Default::default()
        })
    }
}

impl Default for RqbitDesktopConfigListen {
    fn default() -> Self {
        Self {
            enable_tcp: true,
            enable_utp: false,
            enable_upnp_port_forward: true,
            port: 4240,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RqbitDesktopConfigPersistence {
    pub disable: bool,

    #[serde(default)]
    pub folder: PathBuf,

    #[serde(default)]
    pub fastresume: bool,

    /// Deprecated, but keeping for backwards compat for serialized / deserialized config.
    #[serde(default)]
    pub filename: PathBuf,
}

impl RqbitDesktopConfigPersistence {
    pub(crate) fn fix_backwards_compat(&mut self) {
        if self.folder != Path::new("") {
            return;
        }
        if self.filename != Path::new("") {
            if let Some(parent) = self.filename.parent() {
                self.folder = parent.to_owned();
            }
        }
    }
}

impl Default for RqbitDesktopConfigPersistence {
    fn default() -> Self {
        let folder = librqbit::SessionPersistenceConfig::default_json_persistence_folder().unwrap();
        Self {
            disable: false,
            folder,
            fastresume: false,
            filename: PathBuf::new(),
        }
    }
}

#[serde_as]
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RqbitDesktopConfigPeerOpts {
    #[serde_as(as = "serde_with::DurationSeconds")]
    pub connect_timeout: Duration,

    #[serde_as(as = "serde_with::DurationSeconds")]
    pub read_write_timeout: Duration,
}

impl Default for RqbitDesktopConfigPeerOpts {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(2),
            read_write_timeout: Duration::from_secs(10),
        }
    }
}

#[serde_as]
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RqbitDesktopConfigHttpApi {
    pub disable: bool,
    pub listen_addr: SocketAddr,
    pub read_only: bool,
}

impl Default for RqbitDesktopConfigHttpApi {
    fn default() -> Self {
        Self {
            disable: Default::default(),
            listen_addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 3030)),
            read_only: false,
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(default)]
pub struct RqbitDesktopConfigUpnp {
    #[serde(default)]
    pub enable_server: bool,

    #[serde(default)]
    pub server_friendly_name: Option<String>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RqbitDesktopConfig {
    pub default_download_location: PathBuf,

    #[cfg(feature = "disable-upload")]
    #[serde(default)]
    pub disable_upload: bool,

    pub dht: RqbitDesktopConfigDht,
    #[serde(default)]
    pub listen: RqbitDesktopConfigListen,
    pub upnp: RqbitDesktopConfigUpnp,
    pub persistence: RqbitDesktopConfigPersistence,
    pub peer_opts: RqbitDesktopConfigPeerOpts,
    pub http_api: RqbitDesktopConfigHttpApi,

    #[serde(default)]
    pub ratelimits: LimitsConfig,
}

impl Default for RqbitDesktopConfig {
    fn default() -> Self {
        let userdirs = directories::UserDirs::new().expect("directories::UserDirs::new()");
        let download_folder = userdirs
            .download_dir()
            .map(|d| d.to_owned())
            .unwrap_or_else(|| userdirs.home_dir().join("Downloads"));

        Self {
            default_download_location: download_folder,
            dht: Default::default(),
            listen: Default::default(),
            upnp: Default::default(),
            persistence: Default::default(),
            peer_opts: Default::default(),
            http_api: Default::default(),
            ratelimits: Default::default(),
            #[cfg(feature = "disable-upload")]
            disable_upload: false,
        }
    }
}

impl RqbitDesktopConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.upnp.enable_server {
            if self.http_api.disable {
                anyhow::bail!("if UPnP server is enabled, you need to enable the HTTP API also.")
            }
            if self.http_api.listen_addr.ip().is_loopback() {
                anyhow::bail!("if UPnP server is enabled, you need to set HTTP API IP to 0.0.0.0 or at least non-localhost address.")
            }
        }
        Ok(())
    }
}
