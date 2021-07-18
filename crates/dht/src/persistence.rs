// TODO: this now stores only the routing table, but we also need AT LEAST the same socket address...

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use log::{debug, error, info, warn};
use tokio::spawn;

use crate::dht::{Dht, DhtConfig};
use crate::routing_table::RoutingTable;

#[derive(Default, Clone)]
pub struct PersistentDhtConfig {
    pub dump_interval: Option<Duration>,
    pub config_filename: Option<PathBuf>,
}

pub struct PersistentDht {
    // config_filename: PathBuf,
}

fn dump_dht(dht: &Dht, filename: &Path, tempfile_name: &Path) -> anyhow::Result<()> {
    let mut file = OpenOptions::new()
        .truncate(true)
        .create(true)
        .write(true)
        .open(&tempfile_name)
        .with_context(|| format!("error opening {:?}", tempfile_name))?;

    match dht.with_routing_table(|r| serde_json::to_writer(&mut file, r)) {
        Ok(_) => {
            debug!("dumped DHT to {:?}", &tempfile_name);
        }
        Err(e) => {
            return Err(e).with_context(|| {
                format!("error serializing DHT routing table to {:?}", tempfile_name)
            })
        }
    }

    std::fs::rename(tempfile_name, filename)
        .with_context(|| format!("error renaming {:?} to {:?}", tempfile_name, filename))
}

impl PersistentDht {
    pub async fn create(config: Option<PersistentDhtConfig>) -> anyhow::Result<Dht> {
        let mut config = config.unwrap_or_default();
        let config_filename = match config.config_filename.take() {
            Some(config_filename) => config_filename,
            None => {
                let dirs = directories::ProjectDirs::from("com", "rqbit", "dht")
                    .context("cannot determine project directory for com.rqbit.dht")?;
                let path = dirs.cache_dir().join("dht.json");
                info!("will store DHT routing table to {:?} periodically", &path);
                path
            }
        };

        if let Some(parent) = config_filename.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("error creating dir {:?}", &parent))?;
        }

        let routing_table = match OpenOptions::new().read(true).open(&config_filename) {
            Ok(dht_json) => match serde_json::from_reader::<_, RoutingTable>(&dht_json) {
                Ok(r) => {
                    info!("loaded DHT routing table from {:?}", &config_filename);
                    Some(r)
                }
                Err(e) => {
                    warn!(
                        "cannot deserialize routing table from file {:?}: {:#}",
                        &config_filename, e
                    );
                    None
                }
            },
            Err(e) => match e.kind() {
                std::io::ErrorKind::NotFound => None,
                _ => return Err(e).with_context(|| format!("error reading {:?}", config_filename)),
            },
        };
        let peer_id = routing_table.as_ref().map(|r| r.id());
        let dht_config = DhtConfig {
            peer_id,
            routing_table,
            ..Default::default()
        };
        let dht = Dht::with_config(dht_config).await?;

        spawn({
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
                    tokio::time::sleep(dump_interval).await;
                    debug!("dumping DHT to {:?}", &config_filename);

                    match dump_dht(&dht, &config_filename, &tempfile_name) {
                        Ok(_) => debug!("dumped DHT to {:?}", &config_filename),
                        Err(e) => error!("error dumping DHT to {:?}: {:#}", &config_filename, e),
                    }
                }
            }
        });
        Ok(dht)
    }
}
