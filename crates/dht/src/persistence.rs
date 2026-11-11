// TODO: this now stores only the routing table, but we also need AT LEAST the same socket address...

use anyhow::Context;
use futures::FutureExt;
use futures::future::BoxFuture;
use librqbit_core::directories::get_configuration_directory;
use librqbit_core::peer_id::generate_azereus_style;
use librqbit_core::proxy::ProxyConfig;
use librqbit_core::{crate_version, spawn_utils::spawn_with_cancel};
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
    /// Where the datagrams of the DHT went: the proxy URL without credentials, or `None` for
    /// direct (also in files written before this was recorded).
    #[serde(default)]
    egress: Option<String>,
}

/// The egress of `proxy` as it is persisted (see [`DhtSerialize::egress`]).
fn egress(proxy: Option<&ProxyConfig>) -> Option<String> {
    proxy.map(ProxyConfig::url_without_credentials)
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
    let egress = egress(dht.proxy());
    match dht.with_routing_tables(|v4, v6| {
        serde_json::to_writer(
            &mut file,
            &DhtSerialize {
                addr,
                table: v4,
                table_v6: Some(v6),
                peer_store: Some(&dht.peer_store),
                egress,
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
        Self::create_with_config(
            persistence_config,
            port,
            ipv4_only,
            DhtConfig {
                bootstrap_addrs,
                cancellation_token,
                bind_device,
                ..Default::default()
            },
        )
    }

    /// Like [`PersistentDht::create`], with the remaining DHT options taken from `config`.
    ///
    /// The node id, the routing tables and the peer store come from the persisted state when
    /// there is one (they replace those of `config`), and the listen address is computed from
    /// `port`, the persisted port and `ipv4_only` (see [`dht_listen_addr`]). When the state was
    /// saved with another egress (direct, or another proxy than [`DhtConfig::proxy`]), its node
    /// ids are not reused: the nodes are kept around a new id (`config.peer_id` if set).
    #[inline(never)]
    pub fn create_with_config<'a>(
        persistence_config: DhtPersistenceConfig,
        port: Option<u16>,
        ipv4_only: bool,
        config: DhtConfig<'a>,
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
                    .with_context(|| format!("error creating dir {:?}", parent))?;
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
            let current_egress = egress(config.proxy);
            let (stored_port, routing_table, routing_table_v6, peer_store) = match de {
                Some(de) => {
                    // Our node ids say where we are (BEP 42 derives them from the external
                    // address): used on the other side of a proxy change they would link the
                    // two addresses. The nodes are kept, around a fresh id.
                    let (table, table_v6) = if de.egress == current_egress {
                        (de.table, de.table_v6)
                    } else {
                        info!(
                            "the DHT egress changed (proxy {:?} -> {:?}): new node ids",
                            de.egress, current_egress
                        );
                        let id = config
                            .peer_id
                            .unwrap_or_else(|| generate_azereus_style(*b"rQ", crate_version!()));
                        (
                            de.table.with_new_id(id),
                            de.table_v6.map(|t| t.with_new_id(id)),
                        )
                    };
                    (Some(de.addr.port()), Some(table), table_v6, de.peer_store)
                }
                None => (None, None, None, None),
            };

            let listen_addr = dht_listen_addr(port, stored_port, ipv4_only);

            // The restored tables keep our node ids (one per address family, see BEP 42) across
            // restarts.
            let dht_config = DhtConfig {
                peer_id: routing_table.as_ref().map(|r| r.id()).or(config.peer_id),
                routing_table: routing_table.or(config.routing_table),
                routing_table_v6: routing_table_v6.or(config.routing_table_v6),
                listen_addr: Some(listen_addr),
                peer_store: peer_store.or(config.peer_store),
                ..config
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

    // BEP 42: an id that still matches our external address survives a restart, one that does
    // not is replaced once the new address is adopted. Both address families have their own.
    #[tokio::test]
    async fn persisted_node_ids_follow_the_external_address() {
        let silent = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let bootstrap = silent.local_addr().unwrap().to_string();
        let dir = tempfile::TempDir::new().unwrap();
        let filename = dir.path().join("dht.json");
        let start = || {
            PersistentDht::create_with_config(
                DhtPersistenceConfig {
                    dump_interval: Some(Duration::from_secs(3600)),
                    config_filename: Some(filename.clone()),
                },
                // A fresh port each time: the previous instance may still hold its own.
                Some(0),
                true,
                DhtConfig {
                    bootstrap_addrs: Some(vec![bootstrap.clone()]),
                    ..Default::default()
                },
            )
        };
        let ours_v4 = IpAddr::from(Ipv4Addr::new(124, 31, 75, 21));
        let ours_v6 = IpAddr::from(Ipv6Addr::new(0x2001, 0x0db8, 0xaaaa, 0, 0, 0, 0, 1));

        let first = start().await.unwrap();
        first.vote_external_ip(ours_v4);
        first.vote_external_ip(ours_v6);
        let ids = first.get_stats();
        assert_ne!(ids.id, ids.id_v6);
        dump_dht(&first, &filename, &dir.path().join("dht.json.tmp")).unwrap();
        first.cancellation_token().cancel();

        let second = start().await.unwrap();
        assert_eq!(second.get_stats().id, ids.id);
        assert_eq!(second.get_stats().id_v6, ids.id_v6);
        second.vote_external_ip(ours_v4);
        second.vote_external_ip(ours_v6);
        let stats = second.get_stats();
        assert_eq!(stats.external_ip_v4, Some(Ipv4Addr::new(124, 31, 75, 21)));
        assert_eq!(stats.id, ids.id);
        assert_eq!(stats.id_v6, ids.id_v6);
        second.cancellation_token().cancel();

        let third = start().await.unwrap();
        let moved = IpAddr::from(Ipv4Addr::new(21, 75, 31, 124));
        third.vote_external_ip(moved);
        let stats = third.get_stats();
        assert_ne!(stats.id, ids.id);
        assert!(crate::bep42::node_id_matches_ip(&stats.id, moved));
        assert_eq!(stats.id_v6, ids.id_v6);
        third.cancellation_token().cancel();
    }

    // The node ids are not carried over a change of egress (direct <-> a proxy, one proxy ->
    // another): they would link the addresses on both sides. They survive restarts with the same
    // egress.
    #[tokio::test]
    async fn persisted_node_ids_stay_on_their_side_of_a_proxy() {
        use librqbit_core::proxy::test_util::{FakeProxy, FakeProxyKind, FakeProxyOptions};

        let silent = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let bootstrap = silent.local_addr().unwrap().to_string();
        let dir = tempfile::TempDir::new().unwrap();
        let filename = dir.path().join("dht.json");
        let proxy_a = FakeProxy::start(FakeProxyOptions::new(FakeProxyKind::Socks5)).await;
        let proxy_b = FakeProxy::start(FakeProxyOptions::new(FakeProxyKind::Socks5)).await;
        let config_a = ProxyConfig::parse(&proxy_a.url()).unwrap();
        let config_b = ProxyConfig::parse(&proxy_b.url()).unwrap();
        // Starts a node through `proxy`, saves its state and stops it; returns its node ids.
        let run = async |proxy: Option<&ProxyConfig>| {
            let dht = PersistentDht::create_with_config(
                DhtPersistenceConfig {
                    dump_interval: Some(Duration::from_secs(3600)),
                    config_filename: Some(filename.clone()),
                },
                Some(0),
                true,
                DhtConfig {
                    bootstrap_addrs: Some(vec![bootstrap.clone()]),
                    proxy,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            let stats = dht.get_stats();
            dump_dht(&dht, &filename, &dir.path().join("dht.json.tmp")).unwrap();
            dht.cancellation_token().cancel();
            (stats.id, stats.id_v6)
        };

        let direct = run(None).await;
        assert_eq!(run(None).await, direct, "same egress, same ids");

        let through_a = run(Some(&config_a)).await;
        assert_ne!(through_a.0, direct.0);
        assert_ne!(through_a.1, direct.1);
        assert_eq!(run(Some(&config_a)).await, through_a);

        let through_b = run(Some(&config_b)).await;
        assert_ne!(through_b.0, through_a.0);
        assert_ne!(through_b.1, through_a.1);

        let direct_again = run(None).await;
        assert_ne!(direct_again.0, through_b.0);
        assert_ne!(direct_again.0, direct.0);

        // A file written before the egress was recorded counts as direct.
        let json = std::fs::read_to_string(&filename).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value.as_object_mut().unwrap().remove("egress");
        std::fs::write(&filename, value.to_string()).unwrap();
        assert_eq!(run(None).await, direct_again);
    }
}
