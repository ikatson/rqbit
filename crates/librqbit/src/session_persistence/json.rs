use std::{any::TypeId, collections::HashMap, path::PathBuf};

use crate::{
    session::TorrentId, storage::filesystem::FilesystemStorageFactory,
    torrent_state::ManagedTorrentHandle, ManagedTorrentState,
};
use anyhow::{bail, Context};
use async_trait::async_trait;
use futures::{stream::BoxStream, StreamExt};
use itertools::Itertools;
use librqbit_core::Id20;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{trace, warn};

use super::{SerializedTorrent, SessionPersistenceStore};

#[derive(Serialize, Deserialize, Default)]
struct SerializedSessionDatabase {
    torrents: HashMap<usize, SerializedTorrent>,
}

pub struct JsonSessionPersistenceStore {
    output_folder: PathBuf,
    db_filename: PathBuf,
    db_content: tokio::sync::RwLock<SerializedSessionDatabase>,
}

impl std::fmt::Debug for JsonSessionPersistenceStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON database: {:?}", self.output_folder)
    }
}

impl JsonSessionPersistenceStore {
    pub async fn new(output_folder: PathBuf) -> anyhow::Result<Self> {
        let db_filename = output_folder.join("session.json");
        tokio::fs::create_dir_all(&output_folder)
            .await
            .with_context(|| {
                format!(
                    "couldn't create directory {:?} for session storage",
                    output_folder
                )
            })?;

        let db = match tokio::fs::File::open(&db_filename).await {
            Ok(f) => {
                let mut buf = Vec::new();
                let mut rdr = tokio::io::BufReader::new(f);
                rdr.read_to_end(&mut buf).await?;

                serde_json::from_reader(&buf[..]).context("error deserializing session database")?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Default::default(),
            Err(e) => {
                return Err(e).context(format!("error opening session file {:?}", db_filename))
            }
        };

        Ok(Self {
            db_filename,
            output_folder,
            db_content: tokio::sync::RwLock::new(db),
        })
    }

    async fn flush(&self) -> anyhow::Result<()> {
        let tmp_filename = format!("{}.tmp", self.db_filename.to_str().unwrap());
        let mut tmp = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp_filename)
            .await
            .with_context(|| format!("error opening {:?}", tmp_filename))?;

        let mut buf = Vec::new();
        serde_json::to_writer(&mut buf, &*self.db_content.read().await)
            .context("error serializing")?;
        tmp.write_all(&buf)
            .await
            .with_context(|| format!("error writing {tmp_filename:?}"))?;

        tokio::fs::rename(&tmp_filename, &self.db_filename)
            .await
            .context("error renaming persistence file")?;
        trace!(filename=?self.db_filename, "wrote persistence");
        Ok(())
    }

    fn torrent_bytes_filename(&self, info_hash: &Id20) -> PathBuf {
        self.output_folder.join(format!("{:?}.torrent", info_hash))
    }

    async fn update_db(
        &self,
        id: TorrentId,
        torrent: &ManagedTorrentHandle,
        write_torrent_file: bool,
    ) -> anyhow::Result<()> {
        if !torrent
            .storage_factory
            .is_type_id(TypeId::of::<FilesystemStorageFactory>())
        {
            bail!("storages other than FilesystemStorageFactory are not supported");
        }

        let st = SerializedTorrent {
            trackers: torrent
                .info()
                .trackers
                .iter()
                .map(|u| u.to_string())
                .collect(),
            info_hash: torrent.info_hash(),
            // we don't serialize this here, but to a file instead.
            torrent_bytes: Default::default(),
            only_files: torrent.only_files().clone(),
            is_paused: torrent.with_state(|s| matches!(s, ManagedTorrentState::Paused(_))),
            output_folder: torrent.info().options.output_folder.clone(),
        };

        if write_torrent_file && !torrent.info().torrent_bytes.is_empty() {
            let torrent_bytes_file = self.torrent_bytes_filename(&torrent.info_hash());
            match tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&torrent_bytes_file)
                .await
            {
                Ok(mut f) => {
                    if let Err(e) = f.write_all(&torrent.info().torrent_bytes).await {
                        warn!(error=?e, file=?torrent_bytes_file, "error writing torrent bytes")
                    }
                }
                Err(e) => {
                    warn!(error=?e, file=?torrent_bytes_file, "error opening torrent bytes file")
                }
            }
        }

        self.db_content.write().await.torrents.insert(id, st);
        self.flush().await?;

        Ok(())
    }
}

#[async_trait]
impl SessionPersistenceStore for JsonSessionPersistenceStore {
    async fn next_id(&self) -> anyhow::Result<TorrentId> {
        Ok(self
            .db_content
            .read()
            .await
            .torrents
            .keys()
            .copied()
            .max()
            .map(|max| max + 1)
            .unwrap_or(0))
    }

    async fn delete(&self, id: TorrentId) -> anyhow::Result<()> {
        if let Some(t) = self.db_content.write().await.torrents.remove(&id) {
            self.flush().await?;
            let tf = self.torrent_bytes_filename(&t.info_hash);
            if let Err(e) = tokio::fs::remove_file(&tf).await {
                warn!(error=?e, filename=?tf, "error removing torrent file");
            }
        }

        Ok(())
    }

    async fn get(&self, id: TorrentId) -> anyhow::Result<SerializedTorrent> {
        let mut st = self
            .db_content
            .read()
            .await
            .torrents
            .get(&id)
            .cloned()
            .context("no torrent found")?;
        let mut buf = Vec::new();
        let torrent_bytes_filename = self.torrent_bytes_filename(&st.info_hash);
        let mut torrent_bytes_file = match tokio::fs::File::open(&torrent_bytes_filename).await {
            Ok(f) => f,
            Err(e) => {
                warn!(error=?e, filename=?torrent_bytes_filename, "error opening torrent bytes file");
                return Ok(st);
            }
        };
        if let Err(e) = torrent_bytes_file.read_to_end(&mut buf).await {
            warn!(error=?e, filename=?torrent_bytes_filename, "error reading torrent bytes file");
        } else {
            st.torrent_bytes = buf.into();
        }
        return Ok(st);
    }

    async fn stream_all(
        &self,
    ) -> anyhow::Result<BoxStream<'_, anyhow::Result<(TorrentId, SerializedTorrent)>>> {
        let all_ids = self
            .db_content
            .read()
            .await
            .torrents
            .keys()
            .copied()
            .collect_vec();
        Ok(futures::stream::iter(all_ids)
            .then(move |id| async move { self.get(id).await.map(move |st| (id, st)) })
            .boxed())
    }

    async fn store(&self, id: TorrentId, torrent: &ManagedTorrentHandle) -> anyhow::Result<()> {
        self.update_db(id, torrent, true).await
    }

    async fn update_metadata(
        &self,
        id: TorrentId,
        torrent: &ManagedTorrentHandle,
    ) -> anyhow::Result<()> {
        self.update_db(id, torrent, false).await
    }
}
