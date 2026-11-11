use std::{num::NonZeroU32, path::PathBuf};

use crate::{
    api::TorrentIdOrHash, bitv::BitV, bitv_factory::BitVFactory, limits::LimitsConfig,
    session::TorrentId, torrent_state::ManagedTorrentHandle, type_aliases::BF,
};
use anyhow::Context;
use futures::{StreamExt, stream::BoxStream};
use librqbit_core::{Id20, spawn_utils::spawn};
use sqlx::{Pool, Postgres};
use tracing::debug_span;

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
    upload_bps: Option<i64>,
    download_bps: Option<i64>,
    peer_limit: Option<i64>,
}

// NULL is unlimited, and so is a value that does not fit (the column is wider than the rate).
fn bps_from_column(v: Option<i64>) -> Option<NonZeroU32> {
    NonZeroU32::new(u32::try_from(v?).ok()?)
}

fn bps_to_column(v: Option<NonZeroU32>) -> Option<i64> {
    v.map(|v| i64::from(v.get()))
}

// NULL is the session default; a limit too large for the column is stored as the largest value.
fn peer_limit_to_column(v: Option<usize>) -> Option<i64> {
    v.map(|v| i64::try_from(v).unwrap_or(i64::MAX))
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
                ratelimits: LimitsConfig {
                    upload_bps: bps_from_column(self.upload_bps),
                    download_bps: bps_from_column(self.download_bps),
                },
                peer_limit: self.peer_limit.and_then(|v| usize::try_from(v).ok()),
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

        macro_rules! exec {
            ($q:expr) => {
                sqlx::query($q)
                    .execute(&pool)
                    .await
                    .context($q)
                    .context("error running query")?;
            };
        }

        exec!("CREATE SEQUENCE IF NOT EXISTS torrents_id AS integer;");

        exec!(
            "CREATE TABLE IF NOT EXISTS torrents (
          id INTEGER PRIMARY KEY DEFAULT nextval('torrents_id'),
          info_hash BYTEA NOT NULL,
          torrent_bytes BYTEA NOT NULL,
          trackers TEXT[] NOT NULL,
          output_folder TEXT NOT NULL,
          only_files INTEGER[],
          is_paused BOOLEAN NOT NULL
        )"
        );

        exec!("ALTER TABLE torrents ADD COLUMN IF NOT EXISTS have_bitfield BYTEA");
        exec!("ALTER TABLE torrents ADD COLUMN IF NOT EXISTS upload_bps BIGINT");
        exec!("ALTER TABLE torrents ADD COLUMN IF NOT EXISTS download_bps BIGINT");
        exec!("ALTER TABLE torrents ADD COLUMN IF NOT EXISTS peer_limit BIGINT");

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
        let torrent_bytes = torrent
            .metadata
            .load()
            .as_ref()
            .map(|i| i.torrent_bytes.clone())
            .unwrap_or_default();
        let q = "INSERT INTO torrents (id, info_hash, torrent_bytes, trackers, output_folder, only_files, is_paused, upload_bps, download_bps, peer_limit)
        VALUES($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        ON CONFLICT(id) DO NOTHING";
        let ratelimits = torrent.ratelimits();
        sqlx::query(q)
            .bind::<i32>(id.try_into()?)
            .bind(&torrent.info_hash().0[..])
            .bind(torrent_bytes.as_ref())
            .bind(
                torrent
                    .shared()
                    .trackers
                    .iter()
                    .map(|t| t.to_string())
                    .collect::<Vec<_>>(),
            )
            .bind(
                torrent
                    .shared()
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
            .bind(bps_to_column(ratelimits.upload_bps))
            .bind(bps_to_column(ratelimits.download_bps))
            .bind(peer_limit_to_column(torrent.peer_limit()))
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
        let ratelimits = torrent.ratelimits();
        sqlx::query(
            "UPDATE torrents SET only_files = $1, is_paused = $2, upload_bps = $3, download_bps = $4, peer_limit = $5 WHERE id = $6",
        )
        .bind(torrent.only_files().map(|v| {
            v.into_iter()
                .filter_map(|f| f.try_into().ok())
                .collect::<Vec<i32>>()
        }))
        .bind(torrent.is_paused())
        .bind(bps_to_column(ratelimits.upload_bps))
        .bind(bps_to_column(ratelimits.download_bps))
        .bind(peer_limit_to_column(torrent.peer_limit()))
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

struct PgBitfield {
    torrent_id: TorrentIdOrHash,
    inmem: BF,
    pool: Pool<Postgres>,
}

impl BitV for PgBitfield {
    fn as_slice(&self) -> &bitvec::prelude::BitSlice<u8, bitvec::prelude::Msb0> {
        self.inmem.as_bitslice()
    }

    fn as_slice_mut(&mut self) -> &mut bitvec::prelude::BitSlice<u8, bitvec::prelude::Msb0> {
        self.inmem.as_mut_bitslice()
    }

    fn into_dyn(self) -> Box<dyn BitV> {
        Box::new(self)
    }

    fn as_bytes(&self) -> &[u8] {
        self.inmem.as_raw_slice()
    }

    fn flush(&mut self, _flush_async: bool) -> anyhow::Result<()> {
        // TODO: make flush async, and don't spawn this, to avoid allocations and capture the result.
        spawn(
            debug_span!("pg_update_bitfield", id=?self.torrent_id),
            "pg_update_bitfield",
            {
                let hb = self.as_bytes().to_owned();
                let pool = self.pool.clone();
                let torrent_id = self.torrent_id;

                macro_rules! exec {
                    ($q:expr, $bf:expr, $id:expr) => {
                        sqlx::query($q)
                            .bind($bf)
                            .bind($id)
                            .execute(&pool)
                            .await
                            .context($q)
                            .context("error executing query")
                    };
                }

                async move {
                    match torrent_id {
                        TorrentIdOrHash::Id(id) => {
                            let id: i32 = id.try_into()?;
                            exec!(
                                "UPDATE torrents SET have_bitfield = $1 WHERE id = $2",
                                &hb,
                                id
                            )?;
                        }
                        TorrentIdOrHash::Hash(h) => {
                            exec!(
                                "UPDATE torrents SET have_bitfield = $1 WHERE info_hash = $2",
                                &hb,
                                &h.0[..]
                            )?;
                        }
                    };
                    Ok::<_, anyhow::Error>(())
                }
            },
        );
        Ok(())
    }
}

#[async_trait::async_trait]
impl BitVFactory for PostgresSessionStorage {
    async fn load(&self, id: TorrentIdOrHash) -> anyhow::Result<Option<Box<dyn BitV>>> {
        #[derive(sqlx::FromRow)]
        struct HaveBitfield {
            have_bitfield: Option<Vec<u8>>,
        }

        macro_rules! exec {
            ($q:expr, $v:expr) => {
                sqlx::query_as($q)
                    .bind($v)
                    .fetch_one(&self.pool)
                    .await
                    .context($q)
                    .context("error executing query")?
            };
        }

        let hb: HaveBitfield = match id {
            TorrentIdOrHash::Id(id) => {
                let id: i32 = id.try_into()?;
                exec!("SELECT have_bitfield FROM torrents WHERE id = $1", id)
            }
            TorrentIdOrHash::Hash(h) => {
                exec!(
                    "SELECT have_bitfield FROM torrents WHERE info_hash = $1",
                    &h.0[..]
                )
            }
        };

        let hb = hb.have_bitfield;
        Ok(hb.map(|b| {
            PgBitfield {
                torrent_id: id,
                inmem: BF::from_boxed_slice(b.into_boxed_slice()),
                pool: self.pool.clone(),
            }
            .into_dyn()
        }))
    }

    async fn store_initial_check(
        &self,
        id: TorrentIdOrHash,
        b: BF,
    ) -> anyhow::Result<Box<dyn BitV>> {
        let mut bf = PgBitfield {
            torrent_id: id,
            inmem: b,
            pool: self.pool.clone(),
        };
        bf.flush(false)?;
        Ok(bf.into_dyn())
    }

    async fn clear(&self, id: TorrentIdOrHash) -> anyhow::Result<()> {
        macro_rules! exec {
            ($q:expr, $v:expr) => {
                sqlx::query($q)
                    .bind($v)
                    .execute(&self.pool)
                    .await
                    .context($q)
                    .context("error executing query")?
            };
        }

        match id {
            TorrentIdOrHash::Id(id) => {
                let id: i32 = id.try_into()?;
                exec!("UPDATE torrents SET have_bitfield = NULL WHERE id = $1", id);
            }
            TorrentIdOrHash::Hash(h) => {
                exec!(
                    "UPDATE torrents SET have_bitfield = NULL WHERE info_hash = $1",
                    &h.0[..]
                );
            }
        }
        Ok(())
    }
}

// Needs a Postgres server: runs against RQBIT_TEST_POSTGRES_URL, and is skipped (with a note)
// when it isn't set. Works in a schema of its own, dropped when done.
#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use sqlx::{AssertSqlSafe, PgPool};

    use super::*;
    use crate::{
        CreateTorrentOptions, Session, SessionOptions, SessionPersistenceConfig, create_torrent,
        spawn_utils::BlockingSpawner, tests::test_util::create_default_random_dir_with_torrents,
    };

    const ENV: &str = "RQBIT_TEST_POSTGRES_URL";

    async fn session(out: &Path, url: &str) -> anyhow::Result<Arc<Session>> {
        Session::new_with_opts(
            out.to_owned(),
            SessionOptions {
                dht: None,
                persistence: Some(SessionPersistenceConfig::Postgres {
                    connection_string: url.to_owned(),
                }),
                listen: None,
                disable_local_service_discovery: true,
                ..Default::default()
            },
        )
        .await
    }

    fn only(session: &Session) -> anyhow::Result<ManagedTorrentHandle> {
        let mut all = session.with_torrents(|t| t.map(|(_, h)| h.clone()).collect::<Vec<_>>());
        anyhow::ensure!(all.len() == 1, "{} torrents restored", all.len());
        Ok(all.remove(0))
    }

    // A torrent stored by a version without the limit columns loads without limits; limits set
    // later are stored in the new columns, and removing them stores NULL again.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_limits_are_persisted() -> anyhow::Result<()> {
        let Ok(base) = std::env::var(ENV) else {
            eprintln!("skipping test_limits_are_persisted: set {ENV} to run the Postgres tests");
            return Ok(());
        };
        // Made of a fixed name and the process id, so it is safe to splice into the statements
        // (identifiers can't be bound as parameters).
        let schema = format!("rqbit_test_limits_{}", std::process::id());
        let admin = PgPool::connect(&base).await.context("connecting")?;
        sqlx::query(AssertSqlSafe(format!(
            "DROP SCHEMA IF EXISTS {schema} CASCADE"
        )))
        .execute(&admin)
        .await?;
        sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await?;
        // Every connection of the store resolves unqualified names in the test's schema.
        let sep = if base.contains('?') { '&' } else { '?' };
        let url = format!("{base}{sep}options=-c%20search_path%3D{schema}");

        let result = limits_round_trip(&url).await;
        sqlx::query(AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await?;
        result
    }

    async fn limits_round_trip(url: &str) -> anyhow::Result<()> {
        let data = create_default_random_dir_with_torrents(2, 4096, Some("rqbit_pg_limits"));
        let torrent = create_torrent(
            data.path(),
            CreateTorrentOptions {
                piece_length: Some(1024),
                ..Default::default()
            },
            &BlockingSpawner::new(1),
        )
        .await?;

        // The table as a version without the limit columns left it, with one paused torrent.
        let old = PgPool::connect(url).await?;
        for q in [
            "CREATE SEQUENCE torrents_id AS integer",
            "CREATE TABLE torrents (
              id INTEGER PRIMARY KEY DEFAULT nextval('torrents_id'),
              info_hash BYTEA NOT NULL,
              torrent_bytes BYTEA NOT NULL,
              trackers TEXT[] NOT NULL,
              output_folder TEXT NOT NULL,
              only_files INTEGER[],
              is_paused BOOLEAN NOT NULL
            )",
            "ALTER TABLE torrents ADD COLUMN have_bitfield BYTEA",
        ] {
            sqlx::query(q).execute(&old).await?;
        }
        sqlx::query(
            "INSERT INTO torrents (info_hash, torrent_bytes, trackers, output_folder, is_paused)
            VALUES ($1, $2, $3, $4, true)",
        )
        .bind(&torrent.info_hash().0[..])
        .bind(torrent.as_bytes()?.as_ref())
        .bind(Vec::<String>::new())
        .bind(data.path().to_str().context("utf-8")?)
        .execute(&old)
        .await?;
        old.close().await;

        let session = session(data.path(), url).await?;
        let handle = only(&session)?;
        assert_eq!(handle.ratelimits(), LimitsConfig::default());
        assert_eq!(handle.peer_limit(), None);

        let limits = LimitsConfig {
            upload_bps: NonZeroU32::new(100 * 1024),
            download_bps: NonZeroU32::new(200 * 1024),
        };
        session.set_torrent_ratelimits(&handle, limits).await?;
        session.set_torrent_peer_limit(&handle, Some(7)).await?;
        session.stop().await;
        drop(handle);

        let session = self::session(data.path(), url).await?;
        let handle = only(&session)?;
        assert_eq!(handle.ratelimits(), limits);
        assert_eq!(handle.peer_limit(), Some(7));

        session
            .set_torrent_ratelimits(&handle, LimitsConfig::default())
            .await?;
        session.set_torrent_peer_limit(&handle, None).await?;
        session.stop().await;
        drop(handle);

        let session = self::session(data.path(), url).await?;
        let handle = only(&session)?;
        assert_eq!(handle.ratelimits(), LimitsConfig::default());
        assert_eq!(handle.peer_limit(), None);
        session.stop().await;
        Ok(())
    }
}
