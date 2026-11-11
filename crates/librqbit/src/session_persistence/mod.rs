pub mod json;
#[cfg(feature = "postgres")]
pub mod postgres;

use std::{collections::HashSet, path::PathBuf};

use anyhow::Context;
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use librqbit_core::Id20;
use librqbit_core::magnet::Magnet;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    AddTorrent, AddTorrentOptions, bitv_factory::BitVFactory, session::TorrentId,
    torrent_state::ManagedTorrentHandle,
};

#[derive(Serialize, Deserialize, Clone)]
pub struct SerializedTorrent {
    #[serde(
        serialize_with = "serialize_info_hash",
        deserialize_with = "deserialize_info_hash"
    )]
    info_hash: Id20,
    #[serde(skip)]
    torrent_bytes: Bytes,
    trackers: HashSet<String>,
    output_folder: PathBuf,
    only_files: Option<Vec<usize>>,
    is_paused: bool,
    // The web seeds of the torrent, including the ones added at run time and without the ones
    // removed. Missing in older state: the torrent's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    web_seeds: Option<Vec<String>>,
}

impl SerializedTorrent {
    pub fn info_hash(&self) -> &Id20 {
        &self.info_hash
    }
    pub fn into_add_torrent(self) -> anyhow::Result<(AddTorrent<'static>, AddTorrentOptions)> {
        let add_torrent = if !self.torrent_bytes.is_empty() {
            AddTorrent::TorrentFileBytes(self.torrent_bytes)
        } else {
            let magnet = Magnet::from_id20(
                self.info_hash,
                self.trackers.into_iter().collect(),
                self.only_files.clone(),
            )
            .to_string();
            AddTorrent::from_url(magnet)
        };

        let opts = AddTorrentOptions {
            paused: self.is_paused,
            output_folder: Some(
                self.output_folder
                    .to_str()
                    .context("broken path")?
                    .to_owned(),
            ),
            only_files: self.only_files,
            replace_web_seeds: self.web_seeds.is_some(),
            web_seeds: self.web_seeds,
            overwrite: true,
            ..Default::default()
        };

        Ok((add_torrent, opts))
    }
}

// TODO: make this info_hash first, ID-second.
#[async_trait]
pub trait SessionPersistenceStore: core::fmt::Debug + Send + Sync + BitVFactory {
    async fn next_id(&self) -> anyhow::Result<TorrentId>;
    async fn store(&self, id: TorrentId, torrent: &ManagedTorrentHandle) -> anyhow::Result<()>;
    async fn delete(&self, id: TorrentId) -> anyhow::Result<()>;
    async fn get(&self, id: TorrentId) -> anyhow::Result<SerializedTorrent>;
    async fn update_metadata(
        &self,
        id: TorrentId,
        torrent: &ManagedTorrentHandle,
    ) -> anyhow::Result<()>;
    async fn stream_all(
        &self,
    ) -> anyhow::Result<BoxStream<'_, anyhow::Result<(TorrentId, SerializedTorrent)>>>;
}

fn serialize_info_hash<S>(id: &Id20, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    id.as_string().serialize(serializer)
}

fn deserialize_info_hash<'de, D>(deserializer: D) -> Result<Id20, D::Error>
where
    D: Deserializer<'de>,
{
    Id20::deserialize(deserializer)
}

#[cfg(test)]
mod tests {
    use super::SerializedTorrent;

    const INFO_HASH: &str = "a621779b5e3d486e127c3efbca9b6f8d135f52e5";

    #[test]
    fn test_web_seeds_round_trip() {
        let old = format!(
            r#"{{"info_hash":"{INFO_HASH}","trackers":[],"output_folder":"/tmp/t","only_files":null,"is_paused":false}}"#
        );
        // State written before web seeds: the torrent's own are used.
        let mut t: SerializedTorrent = serde_json::from_str(&old).unwrap();
        let (_, opts) = t.clone().into_add_torrent().unwrap();
        assert_eq!(opts.web_seeds, None);
        assert!(!opts.replace_web_seeds);

        // A stored list replaces the torrent's own, even an empty one.
        for urls in [vec![], vec!["http://a/x".to_owned()]] {
            t.web_seeds = Some(urls.clone());
            let json = serde_json::to_string(&t).unwrap();
            let t: SerializedTorrent = serde_json::from_str(&json).unwrap();
            let (_, opts) = t.into_add_torrent().unwrap();
            assert_eq!(opts.web_seeds, Some(urls));
            assert!(opts.replace_web_seeds);
        }
    }
}
