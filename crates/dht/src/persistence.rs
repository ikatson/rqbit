// TODO: this now stores only the routing table, but we also need AT LEAST the same socket address...

use futures::future::BoxFuture;
use futures::FutureExt;
use librqbit_core::directories::get_configuration_directory;
use librqbit_core::spawn_utils::spawn_with_cancel;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{BufReader, BufWriter};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use anyhow::Context;
use tracing::{error, error_span, info, trace, warn};

use crate::peer_store::PeerStore;
use crate::routing_table::RoutingTable;
use crate::{Dht, DhtConfig, DhtState};

#[derive(Default)]
pub struct PersistentDhtConfig {
    pub dump_interval: Option<Duration>,
    pub config_filename: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct DhtSerialize<Table, PeerStore> {
    addr: SocketAddr,
    table: Table,
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
    match dht.with_routing_table(|r| {
        serde_json::to_writer(
            &mut file,
            &DhtSerialize {
                addr,
                table: r,
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
            })
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
    pub fn create(
        config: Option<PersistentDhtConfig>,
        cancellation_token: Option<CancellationToken>,
    ) -> BoxFuture<'static, anyhow::Result<Dht>> {
        async move {
            let mut config = config.unwrap_or_default();
            let config_filename = match config.config_filename.take() {
                Some(config_filename) => config_filename,
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
                                "cannot deserialize routing table: {:#}",
                                e
                            );
                            None
                        }
                    }
                }
                Err(e) => match e.kind() {
                    std::io::ErrorKind::NotFound => None,
                    _ => {
                        return Err(e).with_context(|| format!("error reading {config_filename:?}"))
                    }
                },
            };
            let (listen_addr, routing_table, peer_store) = de
                .map(|de| (Some(de.addr), Some(de.table), de.peer_store))
                .unwrap_or((None, None, None));
            let peer_id = routing_table.as_ref().map(|r| r.id());

            let dht_config = DhtConfig {
                peer_id,
                routing_table,
                listen_addr,
                peer_store,
                cancellation_token,
                ..Default::default()
            };
            let dht = DhtState::with_config(dht_config).await?;
            spawn_with_cancel(
                error_span!("dht_persistence"),
                dht.cancellation_token().clone(),
                {
                    let dht = dht.clone();
                    let dump_interval = config
                        .dump_interval
                        .unwrap_or_else(|| Duration::from_secs(3));
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
