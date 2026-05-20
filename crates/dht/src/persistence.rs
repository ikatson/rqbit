// TODO: this now stores only the routing table, but we also need AT LEAST the same socket address...

use anyhow::Context;
use futures::FutureExt;
use futures::future::BoxFuture;
use librqbit_core::directories::get_configuration_directory;
use librqbit_core::spawn_utils::spawn_with_cancel;
use librqbit_dualstack_sockets::BindDevice;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{BufReader, BufWriter};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use tracing::{debug_span, error, info, trace, warn};

use crate::peer_store::PeerStore;
use crate::routing_table::RoutingTable;
use crate::{Dht, DhtConfig, DhtState};

/// Configuration for DHT persistence (periodic dump of routing table and peer store).
/// When provided, the DHT state will be serialized to disk periodically.
#[derive(Default)]
pub struct DhtPersistenceConfig {
    /// How often to dump state. Defaults to 60s.
    pub dump_interval: Option<Duration>,
    /// Path to the JSON file. Uses OS-specific default if None.
    pub config_filename: Option<PathBuf>,
}

/// Compute the DHT listen address from the explicit/stored port preferences and
/// the session-level `ipv4_only` flag.
///
/// The bind IP is derived solely from `ipv4_only` (`0.0.0.0` if true, `[::]`
/// otherwise); any persisted IP is intentionally ignored. The port is chosen
/// in priority order: explicit -> stored -> 0 (random).
pub fn dht_listen_addr(port: Option<u16>, stored_port: Option<u16>, ipv4_only: bool) -> SocketAddr {
    let ip: IpAddr = if ipv4_only {
        Ipv4Addr::UNSPECIFIED.into()
    } else {
        Ipv6Addr::UNSPECIFIED.into()
    };
    SocketAddr::new(ip, port.or(stored_port).unwrap_or(0))
}

#[derive(Serialize, Deserialize)]
struct DhtSerialize<Table, PeerStore> {
    addr: SocketAddr,
    table: Table,
    // option for backwards compat
    table_v6: Option<Table>,
    peer_store: Option<PeerStore>,
}

pub struct PersistentDht {
    // config_filename: PathBuf,
}

fn dump_dht(dht: &Dht, filename: &Path, tempfile_name: &Path) -> anyhow::Result<()> {
    let file = OpenOptions::new()
        .truncate(true)
        .create(true)
        .write(true)
        .open(tempfile_name)
        .with_context(|| format!("error opening {tempfile_name:?}"))?;
    let mut file = BufWriter::new(file);

    let addr = dht.listen_addr();
    match dht.with_routing_tables(|v4, v6| {
        serde_json::to_writer(
            &mut file,
            &DhtSerialize {
                addr,
                table: v4,
                table_v6: Some(v6),
                peer_store: Some(&dht.peer_store),
            },
        )
    }) {
        Ok(_) => {
            trace!("dumped DHT to {:?}", &tempfile_name);
        }
        Err(e) => {
            return Err(e).with_context(|| {
                format!("error serializing DHT routing table to {tempfile_name:?}")
            });
        }
    }

    std::fs::rename(tempfile_name, filename)
        .with_context(|| format!("error renaming {tempfile_name:?} to {filename:?}"))
}

impl PersistentDht {
    pub fn default_persistence_filename() -> anyhow::Result<PathBuf> {
        let dirs = get_configuration_directory("dht")?;
        let path = dirs.cache_dir().join("dht.json");
        Ok(path)
    }

    #[inline(never)]
    pub fn create<'a>(
        persistence_config: DhtPersistenceConfig,
        port: Option<u16>,
        ipv4_only: bool,
        bootstrap_addrs: Option<Vec<String>>,
        cancellation_token: Option<CancellationToken>,
        bind_device: Option<&'a BindDevice>,
    ) -> BoxFuture<'a, anyhow::Result<Dht>> {
        async move {
            let config_filename = match persistence_config.config_filename {
                Some(f) => f,
                None => Self::default_persistence_filename()?,
            };

            info!(
                filename=?config_filename,
                "will store DHT routing table periodically",
            );

            if let Some(parent) = config_filename.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("error creating dir {:?}", &parent))?;
            }

            let de = match OpenOptions::new().read(true).open(&config_filename) {
                Ok(dht_json) => {
                    let reader = BufReader::new(dht_json);
                    match serde_json::from_reader::<_, DhtSerialize<RoutingTable, PeerStore>>(
                        reader,
                    ) {
                        Ok(r) => {
                            info!(filename=?config_filename, "loaded DHT routing table from");
                            Some(r)
                        }
                        Err(e) => {
                            warn!(
                                filename=?config_filename,
                                "DHT: cannot deserialize routing table: {:#}",
                                e
                            );
                            None
                        }
                    }
                }
                Err(e) => match e.kind() {
                    std::io::ErrorKind::NotFound => None,
                    _ => {
                        return Err(e)
                            .with_context(|| format!("error reading {config_filename:?}"));
                    }
                },
            };
            let (stored_port, routing_table, peer_store) = de
                .map(|de| (Some(de.addr.port()), Some(de.table), de.peer_store))
                .unwrap_or((None, None, None));

            let listen_addr = dht_listen_addr(port, stored_port, ipv4_only);
            let peer_id = routing_table.as_ref().map(|r| r.id());

            let dht_config = DhtConfig {
                peer_id,
                bootstrap_addrs,
                routing_table,
                listen_addr: Some(listen_addr),
                peer_store,
                cancellation_token,
                bind_device,
                ..Default::default()
            };
            let dht = DhtState::with_config(dht_config).await?;
            spawn_with_cancel::<anyhow::Error>(
                debug_span!("dht_persistence"),
                "dht_persistence",
                dht.cancellation_token().clone(),
                {
                    let dht = dht.clone();
                    let dump_interval = persistence_config
                        .dump_interval
                        .unwrap_or_else(|| Duration::from_secs(60));
                    async move {
                        let tempfile_name = {
                            let file_name = format!("dht.json.tmp.{}", std::process::id());
                            let mut tmp = config_filename.clone();
                            tmp.set_file_name(file_name);
                            tmp
                        };

                        loop {
                            trace!("sleeping for {:?}", &dump_interval);
                            tokio::time::sleep(dump_interval).await;

                            match dump_dht(&dht, &config_filename, &tempfile_name) {
                                Ok(_) => trace!(filename=?config_filename, "dumped DHT"),
                                Err(e) => {
                                    error!(filename=?config_filename, "error dumping DHT: {:#}", e)
                                }
                            }
                        }
                    }
                },
            );

            Ok(dht)
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_port_no_stored_v6() {
        let addr = dht_listen_addr(None, None, false);
        assert_eq!(addr.ip(), IpAddr::from(Ipv6Addr::UNSPECIFIED));
        assert_eq!(addr.port(), 0);
    }

    #[test]
    fn no_port_no_stored_v4() {
        let addr = dht_listen_addr(None, None, true);
        assert_eq!(addr.ip(), IpAddr::from(Ipv4Addr::UNSPECIFIED));
        assert_eq!(addr.port(), 0);
    }

    #[test]
    fn explicit_port_v6() {
        let addr = dht_listen_addr(Some(6881), None, false);
        assert_eq!(addr.ip(), IpAddr::from(Ipv6Addr::UNSPECIFIED));
        assert_eq!(addr.port(), 6881);
    }

    #[test]
    fn explicit_port_v4() {
        let addr = dht_listen_addr(Some(6881), None, true);
        assert_eq!(addr.ip(), IpAddr::from(Ipv4Addr::UNSPECIFIED));
        assert_eq!(addr.port(), 6881);
    }

    #[test]
    fn stored_port_only_v6() {
        let addr = dht_listen_addr(None, Some(12345), false);
        assert_eq!(addr.ip(), IpAddr::from(Ipv6Addr::UNSPECIFIED));
        assert_eq!(addr.port(), 12345);
    }

    #[test]
    fn stored_port_only_v4() {
        let addr = dht_listen_addr(None, Some(12345), true);
        assert_eq!(addr.ip(), IpAddr::from(Ipv4Addr::UNSPECIFIED));
        assert_eq!(addr.port(), 12345);
    }

    #[test]
    fn explicit_overrides_stored_v6() {
        let addr = dht_listen_addr(Some(6881), Some(12345), false);
        assert_eq!(addr.ip(), IpAddr::from(Ipv6Addr::UNSPECIFIED));
        assert_eq!(addr.port(), 6881);
    }

    #[test]
    fn explicit_overrides_stored_v4() {
        let addr = dht_listen_addr(Some(6881), Some(12345), true);
        assert_eq!(addr.ip(), IpAddr::from(Ipv4Addr::UNSPECIFIED));
        assert_eq!(addr.port(), 6881);
    }
}
