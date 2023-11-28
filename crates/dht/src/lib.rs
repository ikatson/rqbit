mod bprotocol;
mod dht;
mod persistence;
mod routing_table;
mod utils;

use std::sync::Arc;

pub use crate::dht::DhtStats;
pub use crate::dht::{DhtConfig, DhtState};
pub use librqbit_core::id20::Id20;
pub use persistence::{PersistentDht, PersistentDhtConfig};

pub type Dht = Arc<DhtState>;

pub struct DhtBuilder {}

impl DhtBuilder {
    #[allow(clippy::new_ret_no_self)]
    pub async fn new() -> anyhow::Result<Dht> {
        DhtState::new().await
    }

    pub async fn with_config(config: DhtConfig) -> anyhow::Result<Dht> {
        DhtState::with_config(config).await
    }
}

pub static DHT_BOOTSTRAP: &[&str] = &["dht.transmissionbt.com:6881", "dht.libtorrent.org:25401"];
