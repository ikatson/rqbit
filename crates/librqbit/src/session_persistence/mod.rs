pub mod json;
#[cfg(feature = "postgres")]
pub mod postgres;

use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
};

use anyhow::Context;
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use librqbit_core::Id20;
use librqbit_core::magnet::Magnet;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    AddTorrent, AddTorrentOptions,
    bitv_factory::BitVFactory,
    session::TorrentId,
    torrent_state::{ManagedTorrentHandle, MoveInProgress},
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
    // Session::rename_file: file index -> path relative to output_folder.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    renamed_files: BTreeMap<usize, PathBuf>,
    // A Session::move_storage the process stopped in the middle of; `output_folder` is
    // still the folder it moved from. Undone when the session loads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    move_in_progress: Option<MoveInProgress>,
}

impl SerializedTorrent {
    pub fn info_hash(&self) -> &Id20 {
        &self.info_hash
    }

    /// The files an interrupted move may have left at the place it moved them to, as
    /// (path where the torrent has them, path the move gave them).
    pub(crate) fn interrupted_move(&self) -> Option<Vec<(PathBuf, PathBuf)>> {
        let m = self.move_in_progress.as_ref()?;
        Some(
            m.files
                .iter()
                .map(|rel| (self.output_folder.join(rel), m.to.join(rel)))
                .collect(),
        )
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
            renamed_files: self.renamed_files,
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
