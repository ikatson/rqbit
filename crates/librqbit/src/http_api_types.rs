use std::{net::SocketAddr, str::FromStr, time::Duration};

use itertools::Itertools;
use serde::{Deserialize, Serialize};

use crate::{AddTorrentOptions, PeerConnectionOptions};

pub struct OnlyFiles(Vec<usize>);
pub struct InitialPeers(pub Vec<SocketAddr>);

pub use crate::torrent_state::peer::stats::snapshot::{PeerStatsFilter, PeerStatsSnapshot};

#[derive(Serialize, Deserialize, Default)]
pub struct TorrentAddQueryParams {
    pub overwrite: Option<bool>,
    pub output_folder: Option<String>,
    pub sub_folder: Option<String>,
    pub only_files_regex: Option<String>,
    pub only_files: Option<OnlyFiles>,
    pub peer_connect_timeout: Option<u64>,
    pub peer_read_write_timeout: Option<u64>,
    pub initial_peers: Option<InitialPeers>,
    // Will force interpreting the content as a URL.
    pub is_url: Option<bool>,
    pub list_only: Option<bool>,
}

impl Serialize for OnlyFiles {
    fn serialize<S>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let s = self.0.iter().map(|id| id.to_string()).join(",");
        s.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for OnlyFiles {
    fn deserialize<D>(deserializer: D) -> core::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;

        let s = String::deserialize(deserializer)?;
        let list = s
            .split(',')
            .try_fold(Vec::<usize>::new(), |mut acc, c| match c.parse() {
                Ok(i) => {
                    acc.push(i);
                    Ok(acc)
                }
                Err(_) => Err(D::Error::custom(format!(
                    "only_files: failed to parse {c:?} as integer"
                ))),
            })?;
        if list.is_empty() {
            return Err(D::Error::custom(
                "only_files: should contain at least one file id",
            ));
        }
        Ok(OnlyFiles(list))
    }
}

impl<'de> Deserialize<'de> for InitialPeers {
    fn deserialize<D>(deserializer: D) -> std::prelude::v1::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let string = String::deserialize(deserializer)?;
        let mut addrs = Vec::new();
        for addr_str in string.split(',').filter(|s| !s.is_empty()) {
            addrs.push(SocketAddr::from_str(addr_str).map_err(D::Error::custom)?);
        }
        Ok(InitialPeers(addrs))
    }
}

impl Serialize for InitialPeers {
    fn serialize<S>(&self, serializer: S) -> std::prelude::v1::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0
            .iter()
            .map(|s| s.to_string())
            .join(",")
            .serialize(serializer)
    }
}

impl TorrentAddQueryParams {
    pub fn into_add_torrent_options(self) -> AddTorrentOptions {
        AddTorrentOptions {
            overwrite: self.overwrite.unwrap_or(false),
            only_files_regex: self.only_files_regex,
            only_files: self.only_files.map(|o| o.0),
            output_folder: self.output_folder,
            sub_folder: self.sub_folder,
            list_only: self.list_only.unwrap_or(false),
            initial_peers: self.initial_peers.map(|i| i.0),
            peer_opts: Some(PeerConnectionOptions {
                connect_timeout: self.peer_connect_timeout.map(Duration::from_secs),
                read_write_timeout: self.peer_read_write_timeout.map(Duration::from_secs),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}
