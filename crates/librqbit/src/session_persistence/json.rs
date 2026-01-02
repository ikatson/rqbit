use std::{any::TypeId, collections::HashMap, path::PathBuf};

use crate::{
    api::TorrentIdOrHash,
    bitv::{BitV, DiskBackedBitV},
    bitv_factory::BitVFactory,
    session::TorrentId,
    spawn_utils::BlockingSpawner,
    storage::filesystem::FilesystemStorageFactory,
    torrent_state::ManagedTorrentHandle,
    type_aliases::BF,
};
use anyhow::{Context, bail};
use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use itertools::Itertools;
use librqbit_core::Id20;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, trace, warn};

use super::{SerializedTorrent, SessionPersistenceStore};

#[derive(Serialize, Deserialize, Default)]
struct SerializedSessionDatabase {
    torrents: HashMap<usize, SerializedTorrent>,
}

pub struct JsonSessionPersistenceStore {
    output_folder: PathBuf,
    db_filename: PathBuf,
    db_content: tokio::sync::RwLock<SerializedSessionDatabase>,
    spawner: BlockingSpawner,
}

impl std::fmt::Debug for JsonSessionPersistenceStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON database: {:?}", self.output_folder)
    }
}

impl JsonSessionPersistenceStore {
    pub async fn new(output_folder: PathBuf, spawner: BlockingSpawner) -> anyhow::Result<Self> {
        let db_filename = output_folder.join("session.json");
        tokio::fs::create_dir_all(&output_folder)
            .await
            .with_context(|| {
                format!("couldn't create directory {output_folder:?} for session storage")
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
                return Err(e).context(format!("error opening session file {db_filename:?}"));
            }
        };

        Ok(Self {
            db_filename,
            output_folder,
            db_content: tokio::sync::RwLock::new(db),
            spawner,
        })
    }

    async fn to_hash(&self, id: TorrentIdOrHash) -> anyhow::Result<Id20> {
        match id {
            TorrentIdOrHash::Id(id) => self
                .db_content
                .read()
                .await
                .torrents
                .get(&id)
                .map(|v| *v.info_hash())
                .context("not found"),
            TorrentIdOrHash::Hash(h) => Ok(h),
        }
    }

    async fn flush(&self) -> anyhow::Result<()> {
        // we don't need the write lock technically, but we need to stop concurrent modifications
        let db_content = self.db_content.write().await;
        let tmp_filename = format!("{}.tmp", self.db_filename.to_str().unwrap());
        let mut tmp = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp_filename)
            .await
            .with_context(|| format!("error opening {tmp_filename:?}"))?;
        trace!(?tmp_filename, "opened temp file");

        let mut buf = Vec::new();
        serde_json::to_writer(&mut buf, &*db_content).context("error serializing")?;

        trace!(?tmp_filename, "serialized DB as JSON");
        tmp.write_all(&buf)
            .await
            .with_context(|| format!("error writing {tmp_filename:?}"))?;
        trace!(?tmp_filename, "wrote to temp file");

        tokio::fs::rename(&tmp_filename, &self.db_filename)
            .await
            .context("error renaming persistence file")?;
        trace!(filename=?self.db_filename, "wrote persistence");
        Ok(())
    }

    fn torrent_bytes_filename(&self, info_hash: &Id20) -> PathBuf {
        self.output_folder.join(format!("{info_hash:?}.torrent"))
    }

    fn bitv_filename(&self, info_hash: &Id20) -> PathBuf {
        self.output_folder.join(format!("{info_hash:?}.bitv"))
    }

    async fn update_db(
        &self,
        id: TorrentId,
        torrent: &ManagedTorrentHandle,
        write_torrent_file: bool,
    ) -> anyhow::Result<()> {
        if !torrent
            .shared
            .storage_factory
            .is_type_id(TypeId::of::<FilesystemStorageFactory>())
        {
            bail!("storages other than FilesystemStorageFactory are not supported");
        }

        let st = SerializedTorrent {
            trackers: torrent
                .shared()
                .trackers
                .iter()
                .map(|u| u.to_string())
                .collect(),
            info_hash: torrent.info_hash(),
            // we don't serialize this here, but to a file instead.
            torrent_bytes: Default::default(),
            only_files: torrent.only_files().clone(),
            is_paused: torrent.is_paused(),
            output_folder: torrent.shared().options.output_folder.clone(),
        };

        let torrent_bytes = torrent
            .metadata
            .load()
            .as_ref()
            .map(|i| i.torrent_bytes.clone())
            .unwrap_or_default();

        if write_torrent_file && !torrent_bytes.is_empty() {
            let torrent_bytes_file = self.torrent_bytes_filename(&torrent.info_hash());
            match tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&torrent_bytes_file)
                .await
            {
                Ok(mut f) => {
                    if let Err(e) = f.write_all(&torrent_bytes).await {
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

#[async_trait::async_trait]
impl BitVFactory for JsonSessionPersistenceStore {
    async fn load(&self, id: TorrentIdOrHash) -> anyhow::Result<Option<Box<dyn BitV>>> {
        let h = self.to_hash(id).await?;
        let filename = self.bitv_filename(&h);
        match DiskBackedBitV::new(filename, self.spawner.clone()).await {
            Ok(bitv) => Ok(Some(bitv.into_dyn())),
            Err(e) => {
                if let Some(e) = e.downcast_ref::<std::io::Error>()
                    && matches!(e.kind(), std::io::ErrorKind::NotFound)
                {
                    return Ok(None);
                }
                return Err(e);
            }
        }
    }

    async fn clear(&self, id: TorrentIdOrHash) -> anyhow::Result<()> {
        let h = self.to_hash(id).await?;
        let filename = self.bitv_filename(&h);
        tokio::fs::remove_file(&filename)
            .await
            .with_context(|| format!("error removing {filename:?}"))
    }

    async fn store_initial_check(
        &self,
        id: TorrentIdOrHash,
        b: BF,
    ) -> anyhow::Result<Box<dyn BitV>> {
        let h = self.to_hash(id).await?;
        let filename = self.bitv_filename(&h);
        let tmp_filename = format!("{}.tmp", filename.to_str().context("bug")?);
        let mut dst = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_filename)
            .await
            .with_context(|| format!("error opening {filename:?}"))?;
        tokio::io::copy(&mut b.as_raw_slice(), &mut dst)
            .await
            .context("error writing bitslice to {filename:?}")?;
        tokio::fs::rename(&tmp_filename, &filename)
            .await
            .with_context(|| format!("error renaming {tmp_filename:?} to {filename:?}"))?;
        trace!(?filename, "stored initial check bitfield");
        Ok(DiskBackedBitV::new(filename.clone(), self.spawner.clone())
            .await
            .with_context(|| format!("error constructing MmapBitV from file {filename:?}"))?
            .into_dyn())
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
        debug!(?id, "attempting to delete");
        // BIG NOTE: DO NOT inline this variable. Otherwise Rust doesn't drop the lock and it deadlocks
        // when calling flush - because let bindings prolong the duration.
        let removed = self.db_content.write().await.torrents.remove(&id);
        if let Some(t) = removed {
            debug!(?id, "deleted from in-memory db, flushing");
            self.flush().await?;
            for tf in [
                self.torrent_bytes_filename(&t.info_hash),
                self.bitv_filename(&t.info_hash),
            ] {
                if let Err(e) = tokio::fs::remove_file(&tf).await {
                    warn!(error=?e, filename=?tf, "error removing");
                } else {
                    debug!(filename=?tf, "removed");
                }
            }
        } else {
            bail!("error deleting: didn't find torrent id={id}")
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
