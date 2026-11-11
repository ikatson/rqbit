use std::path::PathBuf;

use crate::{
    PiecePicking, api::TorrentIdOrHash, bitv::BitV, bitv_factory::BitVFactory, session::TorrentId,
    torrent_state::ManagedTorrentHandle, type_aliases::BF,
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
    sequential: bool,
    first_last_pieces_first: bool,
    // NULL in rows written before priorities.
    file_priorities: Option<Vec<i16>>,
    // Explicit piece priorities as two parallel arrays: piece index and level.
    piece_priority_pieces: Vec<i64>,
    piece_priority_levels: Vec<i16>,
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
                piece_picking: PiecePicking {
                    sequential: self.sequential,
                    first_last_pieces_first: self.first_last_pieces_first,
                },
                // A level that doesn't fit a u8 can't have been written by us: fall back to
                // `only_files` rather than shift the other files' levels. Piece entries are
                // independent, so a bad one is just dropped.
                file_priorities: self.file_priorities.and_then(|v| {
                    v.into_iter()
                        .map(|l| u8::try_from(l).ok())
                        .collect::<Option<Vec<u8>>>()
                }),
                piece_priorities: self
                    .piece_priority_pieces
                    .into_iter()
                    .zip(self.piece_priority_levels)
                    .filter_map(|(p, l)| Some((u32::try_from(p).ok()?, u8::try_from(l).ok()?)))
                    .collect(),
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
        exec!(
            "ALTER TABLE torrents ADD COLUMN IF NOT EXISTS sequential BOOLEAN NOT NULL DEFAULT FALSE"
        );
        exec!(
            "ALTER TABLE torrents ADD COLUMN IF NOT EXISTS first_last_pieces_first BOOLEAN NOT NULL DEFAULT FALSE"
        );
        exec!("ALTER TABLE torrents ADD COLUMN IF NOT EXISTS file_priorities SMALLINT[]");
        exec!(
            "ALTER TABLE torrents ADD COLUMN IF NOT EXISTS piece_priority_pieces BIGINT[] NOT NULL DEFAULT '{}'"
        );
        exec!(
            "ALTER TABLE torrents ADD COLUMN IF NOT EXISTS piece_priority_levels SMALLINT[] NOT NULL DEFAULT '{}'"
        );

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
        let q = "INSERT INTO torrents (id, info_hash, torrent_bytes, trackers, output_folder, only_files, is_paused, sequential, first_last_pieces_first, file_priorities, piece_priority_pieces, piece_priority_levels)
        VALUES($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
        ON CONFLICT(id) DO NOTHING";
        let piece_picking = torrent.piece_picking();
        let (pieces, levels) = piece_priority_columns(torrent);
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
            .bind(piece_picking.sequential)
            .bind(piece_picking.first_last_pieces_first)
            .bind(file_priority_column(torrent))
            .bind(pieces)
            .bind(levels)
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
        let piece_picking = torrent.piece_picking();
        let (pieces, levels) = piece_priority_columns(torrent);
        sqlx::query(
            "UPDATE torrents SET only_files = $1, is_paused = $2, sequential = $3, first_last_pieces_first = $4, file_priorities = $5, piece_priority_pieces = $6, piece_priority_levels = $7 WHERE id = $8",
        )
        .bind(torrent.only_files().map(|v| {
            v.into_iter()
                .filter_map(|f| f.try_into().ok())
                .collect::<Vec<i32>>()
        }))
        .bind(torrent.is_paused())
        .bind(piece_picking.sequential)
        .bind(piece_picking.first_last_pieces_first)
        .bind(file_priority_column(torrent))
        .bind(pieces)
        .bind(levels)
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

fn file_priority_column(torrent: &ManagedTorrentHandle) -> Vec<i16> {
    torrent
        .file_priorities()
        .into_iter()
        .map(i16::from)
        .collect()
}

fn piece_priority_columns(torrent: &ManagedTorrentHandle) -> (Vec<i64>, Vec<i16>) {
    torrent
        .piece_priority_overrides()
        .into_iter()
        .map(|(piece, level)| (i64::from(piece), i16::from(level)))
        .unzip()
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

// These tests need a Postgres server: they run against RQBIT_TEST_POSTGRES_URL and are skipped
// (with a note) when it isn't set. Each test works in a schema of its own, so they can run in
// parallel against one database.
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::TryStreamExt;
    use sqlx::{AssertSqlSafe, PgPool};
    use tempfile::TempDir;

    use super::*;
    use crate::{
        AddTorrent, AddTorrentOptions, CreateTorrentOptions, Session, SessionOptions,
        create_torrent, spawn_utils::BlockingSpawner,
        tests::test_util::create_default_random_dir_with_torrents,
    };

    const ENV: &str = "RQBIT_TEST_POSTGRES_URL";

    // A schema of its own for one test, dropped (with everything in it) when done.
    struct Scratch {
        admin: PgPool,
        schema: String,
        url: String,
    }

    impl Scratch {
        async fn new(test: &str) -> anyhow::Result<Option<Self>> {
            let Ok(base) = std::env::var(ENV) else {
                eprintln!("skipping {test}: set {ENV} to run the Postgres tests");
                return Ok(None);
            };
            // The name is made of a fixed test name and the process id, so it is safe to splice
            // into the statements (identifiers can't be bound as parameters).
            let schema = format!("rqbit_test_{test}_{}", std::process::id());
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
            Ok(Some(Self { admin, schema, url }))
        }

        async fn store(&self) -> anyhow::Result<PostgresSessionStorage> {
            PostgresSessionStorage::new(&self.url).await
        }

        async fn drop_schema(self) -> anyhow::Result<()> {
            sqlx::query(AssertSqlSafe(format!(
                "DROP SCHEMA {} CASCADE",
                self.schema
            )))
            .execute(&self.admin)
            .await?;
            Ok(())
        }
    }

    fn session_opts() -> SessionOptions {
        SessionOptions {
            dht: None,
            persistence: None,
            listen: None,
            ..Default::default()
        }
    }

    async fn add_paused(
        session: &Arc<Session>,
        torrent: AddTorrent<'_>,
        opts: AddTorrentOptions,
    ) -> anyhow::Result<ManagedTorrentHandle> {
        let handle = session
            .add_torrent(
                torrent,
                Some(AddTorrentOptions {
                    paused: true,
                    overwrite: true,
                    ..opts
                }),
            )
            .await?
            .into_handle()
            .context("into_handle")?;
        handle.wait_until_initialized().await?;
        Ok(handle)
    }

    async fn load_all(
        store: &PostgresSessionStorage,
    ) -> anyhow::Result<Vec<(TorrentId, SerializedTorrent)>> {
        store.stream_all().await?.try_collect().await
    }

    // The row of one torrent, as the session reads it on startup.
    async fn load(
        store: &PostgresSessionStorage,
        id: TorrentId,
    ) -> anyhow::Result<SerializedTorrent> {
        load_all(store)
            .await?
            .into_iter()
            .find_map(|(i, st)| (i == id).then_some(st))
            .context("no such row")
    }

    // File and piece priorities reach the row through store and update_metadata, read back the
    // same, and a torrent added from the row gets them again.
    #[tokio::test]
    async fn test_priorities_round_trip() -> anyhow::Result<()> {
        let Some(scratch) = Scratch::new("priorities").await? else {
            return Ok(());
        };
        let store = scratch.store().await?;

        // Two files of four 1 KiB pieces each: pieces 0..4 are file 0, 4..8 file 1.
        let data: TempDir = create_default_random_dir_with_torrents(2, 4096, Some("rqbit_pg_prio"));
        let torrent = create_torrent(
            data.path(),
            CreateTorrentOptions {
                piece_length: Some(1024),
                ..Default::default()
            },
            &BlockingSpawner::new(1),
        )
        .await?;
        let output_folder = Some(data.path().to_str().context("utf-8")?.to_owned());
        let session = Session::new_with_opts(data.path().to_owned(), session_opts()).await?;
        let handle = add_paused(
            &session,
            AddTorrent::from_bytes(torrent.as_bytes()?),
            AddTorrentOptions {
                output_folder: output_folder.clone(),
                ..Default::default()
            },
        )
        .await?;

        let id = store.next_id().await?;
        store.store(id, &handle).await?;
        let st = load(&store, id).await?;
        assert_eq!(st.file_priorities, Some(vec![1, 1]));
        assert!(st.piece_priorities.is_empty());
        assert_eq!(st.only_files, None);

        session
            .set_file_priorities(&handle, &[(0, 0), (1, 7)])
            .await?;
        session
            .set_piece_priorities(&handle, &[(2, Some(6)), (5, Some(0))])
            .await?;
        store.update_metadata(id, &handle).await?;
        let check = |st: &SerializedTorrent| {
            assert_eq!(st.file_priorities, Some(vec![0, 7]));
            assert_eq!(st.piece_priorities, vec![(2, 6), (5, 0)]);
            assert_eq!(st.only_files, Some(vec![1]));
        };
        check(&load(&store, id).await?);

        // A fresh insert carries the same priorities.
        let id2 = store.next_id().await?;
        store.store(id2, &handle).await?;
        let all = load_all(&store).await?;
        assert_eq!(all.len(), 2);
        for (_, st) in &all {
            check(st);
        }

        // Added back from the row, the torrent has the same levels.
        let (add, opts) = load(&store, id2).await?.into_add_torrent()?;
        let restored_dir = TempDir::with_prefix("rqbit_pg_prio_restored")?;
        let restored_session =
            Session::new_with_opts(restored_dir.path().to_owned(), session_opts()).await?;
        let restored = add_paused(&restored_session, add, opts).await?;
        assert_eq!(restored.file_priorities(), vec![0, 7]);
        assert_eq!(restored.piece_priority_overrides(), vec![(2, 6), (5, 0)]);
        assert_eq!(restored.piece_priorities(), vec![0, 0, 6, 0, 7, 0, 7, 7]);

        // Clearing the piece overrides empties the columns again.
        session
            .set_piece_priorities(&handle, &[(2, None), (5, None)])
            .await?;
        store.update_metadata(id, &handle).await?;
        assert!(load(&store, id).await?.piece_priorities.is_empty());

        scratch.drop_schema().await
    }

    // A table created before priorities existed: the store adds the columns and loads the old
    // row from only_files. Values that can't have been written by the store are not trusted: a
    // bad file level falls back to only_files, a bad piece entry is dropped.
    #[tokio::test]
    async fn test_table_from_before_priorities() -> anyhow::Result<()> {
        let Some(scratch) = Scratch::new("legacy_priorities").await? else {
            return Ok(());
        };
        let legacy = PgPool::connect(&scratch.url).await?;
        for q in [
            "CREATE SEQUENCE IF NOT EXISTS torrents_id AS integer",
            "CREATE TABLE IF NOT EXISTS torrents (
              id INTEGER PRIMARY KEY DEFAULT nextval('torrents_id'),
              info_hash BYTEA NOT NULL,
              torrent_bytes BYTEA NOT NULL,
              trackers TEXT[] NOT NULL,
              output_folder TEXT NOT NULL,
              only_files INTEGER[],
              is_paused BOOLEAN NOT NULL
            )",
            "ALTER TABLE torrents ADD COLUMN IF NOT EXISTS have_bitfield BYTEA",
            "ALTER TABLE torrents ADD COLUMN IF NOT EXISTS sequential BOOLEAN NOT NULL DEFAULT FALSE",
            "ALTER TABLE torrents ADD COLUMN IF NOT EXISTS first_last_pieces_first BOOLEAN NOT NULL DEFAULT FALSE",
        ] {
            sqlx::query(q).execute(&legacy).await?;
        }
        sqlx::query(
            "INSERT INTO torrents (id, info_hash, torrent_bytes, trackers, output_folder, only_files, is_paused)
            VALUES (5, $1, $2, $3, '/old/folder', $4, TRUE)",
        )
        .bind(&[7u8; 20][..])
        .bind(&b"torrent"[..])
        .bind(Vec::<String>::new())
        .bind(vec![0i32, 2])
        .execute(&legacy)
        .await?;
        legacy.close().await;

        let store = scratch.store().await?;
        let st = load(&store, 5).await?;
        assert_eq!(st.only_files, Some(vec![0, 2]));
        assert!(st.file_priorities.is_none());
        assert!(st.piece_priorities.is_empty());

        sqlx::query(
            "UPDATE torrents SET file_priorities = '{1,300,7}', piece_priority_pieces = '{3,-1,4}',
            piece_priority_levels = '{6,6,256}' WHERE id = 5",
        )
        .execute(&store.pool)
        .await?;
        let all = load_all(&store).await?;
        assert_eq!(all.len(), 1);
        let st = &all[0].1;
        assert_eq!(st.only_files, Some(vec![0, 2]));
        assert!(st.file_priorities.is_none());
        assert_eq!(st.piece_priorities, vec![(3, 6)]);

        scratch.drop_schema().await
    }
}
