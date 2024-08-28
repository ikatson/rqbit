use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    path::{Path, PathBuf},
    time::Duration,
};

use librqbit::dht::PersistentDht;
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
pub struct RqbitDesktopConfigTcpListen {
    pub disable: bool,
    pub min_port: u16,
    pub max_port: u16,
}

impl Default for RqbitDesktopConfigTcpListen {
    fn default() -> Self {
        Self {
            disable: false,
            // TODO: use consts from librqbit
            min_port: 4240,
            max_port: 4260,
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
    // rename for backwards compat
    #[serde(rename = "disable")]
    pub disable_tcp_port_forward: bool,

    #[serde(default)]
    pub enable_server: bool,

    #[serde(default)]
    pub server_friendly_name: Option<String>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RqbitDesktopConfig {
    pub default_download_location: PathBuf,
    pub dht: RqbitDesktopConfigDht,
    pub tcp_listen: RqbitDesktopConfigTcpListen,
    pub upnp: RqbitDesktopConfigUpnp,
    pub persistence: RqbitDesktopConfigPersistence,
    pub peer_opts: RqbitDesktopConfigPeerOpts,
    pub http_api: RqbitDesktopConfigHttpApi,
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
            tcp_listen: Default::default(),
            upnp: Default::default(),
            persistence: Default::default(),
            peer_opts: Default::default(),
            http_api: Default::default(),
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
