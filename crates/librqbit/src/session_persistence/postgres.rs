use std::path::PathBuf;

use crate::{
    api::TorrentIdOrHash, bitv::BitV, bitv_factory::BitVFactory, session::TorrentId,
    torrent_state::ManagedTorrentHandle, type_aliases::BF,
};
use anyhow::Context;
use futures::{stream::BoxStream, StreamExt};
use librqbit_core::Id20;
use sqlx::{Pool, Postgres};
use tracing::debug;

use super::{SerializedTorrent, SessionPersistenceStore};

#[derive(Debug)]
pub struct PostgresSessionStorage {
    pool: Pool<Postgres>,
}

#[derive(sqlx::FromRow)]
struct TorrentsTableRecord {
    id: i32,
    info_hash: Vec<u8>,
    torrent_bytes: Vec<u8>,
    trackers: Vec<String>,
    output_folder: String,
    only_files: Option<Vec<i32>>,
    is_paused: bool,
}

impl TorrentsTableRecord {
    fn into_serialized_torrent(self) -> Option<(TorrentId, SerializedTorrent)> {
        Some((
            self.id as TorrentId,
            SerializedTorrent {
                info_hash: Id20::from_bytes(&self.info_hash).ok()?,
                torrent_bytes: self.torrent_bytes.into(),
                trackers: self.trackers.into_iter().collect(),
                output_folder: PathBuf::from(self.output_folder),
                only_files: self
                    .only_files
                    .map(|v| v.into_iter().map(|v| v as usize).collect()),
                is_paused: self.is_paused,
            },
        ))
    }
}

impl PostgresSessionStorage {
    pub async fn new(connection_string: &str) -> anyhow::Result<Self> {
        use sqlx::postgres::PgPoolOptions;

        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(connection_string)
            .await?;

        sqlx::query("CREATE SEQUENCE IF NOT EXISTS torrents_id AS integer;")
            .execute(&pool)
            .await
            .context("error executing CREATE SEQUENCE")?;

        let create_q = "CREATE TABLE IF NOT EXISTS torrents (
          id INTEGER PRIMARY KEY DEFAULT nextval('torrents_id'),
          info_hash BYTEA NOT NULL,
          torrent_bytes BYTEA NOT NULL,
          trackers TEXT[] NOT NULL,
          output_folder TEXT NOT NULL,
          only_files INTEGER[],
          is_paused BOOLEAN NOT NULL
        )";
        sqlx::query(create_q)
            .execute(&pool)
            .await
            .context("error executing CREATE TABLE")?;

        Ok(Self { pool })
    }
}

#[async_trait::async_trait]
impl SessionPersistenceStore for PostgresSessionStorage {
    async fn next_id(&self) -> anyhow::Result<TorrentId> {
        let (id,): (i32,) = sqlx::query_as("SELECT nextval('torrents_id')::int")
            .fetch_one(&self.pool)
            .await
            .context("error executing SELECT nextval")?;
        Ok(id as usize)
    }

    async fn store(&self, id: TorrentId, torrent: &ManagedTorrentHandle) -> anyhow::Result<()> {
        let torrent_bytes: &[u8] = &torrent.info().torrent_bytes;
        let q = "INSERT INTO torrents (id, info_hash, torrent_bytes, trackers, output_folder, only_files, is_paused)
        VALUES($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT(id) DO NOTHING";
        sqlx::query(q)
            .bind::<i32>(id.try_into()?)
            .bind(&torrent.info_hash().0[..])
            .bind(torrent_bytes)
            .bind(torrent.info().trackers.iter().cloned().collect::<Vec<_>>())
            .bind(
                torrent
                    .info()
                    .options
                    .output_folder
                    .to_str()
                    .context("output_folder")?
                    .to_owned(),
            )
            .bind(torrent.only_files().map(|o| {
                o.into_iter()
                    .filter_map(|o| o.try_into().ok())
                    .collect::<Vec<i32>>()
            }))
            .bind(torrent.is_paused())
            .execute(&self.pool)
            .await
            .context("error executing INSERT INTO torrents")?;
        Ok(())
    }

    async fn delete(&self, id: TorrentId) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM torrents WHERE id = $1")
            .bind::<i32>(id.try_into()?)
            .execute(&self.pool)
            .await
            .context("error executing DELETE FROM torrents")?;
        Ok(())
    }

    async fn get(&self, id: TorrentId) -> anyhow::Result<SerializedTorrent> {
        let row = sqlx::query_as::<_, TorrentsTableRecord>("SELECT * FROM torrents WHERE id = ?")
            .bind::<i32>(id.try_into()?)
            .fetch_one(&self.pool)
            .await
            .context("error executing SELECT * FROM torrents")?;
        row.into_serialized_torrent()
            .context("bug")
            .map(|(_, st)| st)
    }

    async fn update_metadata(
        &self,
        id: TorrentId,
        torrent: &ManagedTorrentHandle,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE torrents SET only_files = $1, is_paused = $2 WHERE id = $3")
            .bind(torrent.only_files().map(|v| {
                v.into_iter()
                    .filter_map(|f| f.try_into().ok())
                    .collect::<Vec<i32>>()
            }))
            .bind(torrent.is_paused())
            .bind::<i32>(id.try_into()?)
            .execute(&self.pool)
            .await
            .context("error executing UPDATE torrents")?;
        Ok(())
    }

    async fn stream_all(
        &self,
    ) -> anyhow::Result<BoxStream<'_, anyhow::Result<(TorrentId, SerializedTorrent)>>> {
        let torrents = sqlx::query_as::<_, TorrentsTableRecord>("SELECT * FROM torrents")
            .fetch_all(&self.pool)
            .await
            .context("error executing SELECT * FROM torrents")?
            .into_iter()
            .filter_map(TorrentsTableRecord::into_serialized_torrent)
            .map(Ok);
        Ok(futures::stream::iter(torrents).boxed())
    }
}

#[async_trait::async_trait]
impl BitVFactory for PostgresSessionStorage {
    async fn load(&self, _: TorrentIdOrHash) -> anyhow::Result<Option<Box<dyn BitV>>> {
        debug!("BitVFactory not implemented for PostgresSessionStorage: fastresume not available");
        Ok(None)
    }

    async fn store_initial_check(
        &self,
        _: TorrentIdOrHash,
        b: BF,
    ) -> anyhow::Result<Box<dyn BitV>> {
        Ok(b.into_dyn())
    }
}
