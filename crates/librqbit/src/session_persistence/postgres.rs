use std::{collections::BTreeMap, path::PathBuf};

use crate::{
    api::TorrentIdOrHash, bitv::BitV, bitv_factory::BitVFactory, session::TorrentId,
    torrent_state::ManagedTorrentHandle, type_aliases::BF,
};
use anyhow::Context;
use futures::{StreamExt, stream::BoxStream};
use librqbit_core::{Id20, spawn_utils::spawn};
use sqlx::{Pool, Postgres};
use tracing::{debug_span, warn};

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
    // Session::rename_file, as a JSON object (file index -> relative path). NULL if none.
    renamed_files: Option<String>,
    // A Session::move_storage the process stopped in the middle of, as JSON. NULL if none.
    move_in_progress: Option<String>,
}

impl TorrentsTableRecord {
    fn into_serialized_torrent(self) -> Option<(TorrentId, SerializedTorrent)> {
        let renamed_files = match self.renamed_files.as_deref() {
            None => BTreeMap::new(),
            Some(s) => match serde_json::from_str(s) {
                Ok(r) => r,
                Err(e) => {
                    warn!(id = self.id, "ignoring broken renamed_files {s:?}: {e:#}");
                    BTreeMap::new()
                }
            },
        };
        let move_in_progress = self.move_in_progress.as_deref().and_then(|s| {
            serde_json::from_str(s)
                .inspect_err(|e| {
                    warn!(
                        id = self.id,
                        "ignoring broken move_in_progress {s:?}: {e:#}"
                    )
                })
                .ok()
        });
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
                renamed_files,
                move_in_progress,
            },
        ))
    }
}

// The renamed_files column of a torrent: NULL when nothing is renamed.
fn renamed_files_column(torrent: &ManagedTorrentHandle) -> anyhow::Result<Option<String>> {
    let renamed = torrent.shared().renamed_files();
    if renamed.is_empty() {
        return Ok(None);
    }
    serde_json::to_string(&renamed)
        .map(Some)
        .context("error serializing renamed_files")
}

// The move_in_progress column of a torrent: NULL when no move is underway.
fn move_in_progress_column(torrent: &ManagedTorrentHandle) -> anyhow::Result<Option<String>> {
    torrent
        .shared()
        .move_in_progress()
        .map(|m| serde_json::to_string(&m).context("error serializing move_in_progress"))
        .transpose()
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
        exec!("ALTER TABLE torrents ADD COLUMN IF NOT EXISTS renamed_files TEXT");
        exec!("ALTER TABLE torrents ADD COLUMN IF NOT EXISTS move_in_progress TEXT");

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
        let q = "INSERT INTO torrents (id, info_hash, torrent_bytes, trackers, output_folder, only_files, is_paused, renamed_files, move_in_progress)
        VALUES($1, $2, $3, $4, $5, $6, $7, $8, $9)
        ON CONFLICT(id) DO NOTHING";
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
                    .output_folder()
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
            .bind(renamed_files_column(torrent)?)
            .bind(move_in_progress_column(torrent)?)
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
        let row = sqlx::query_as::<_, TorrentsTableRecord>("SELECT * FROM torrents WHERE id = $1")
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
        // The output folder changes with Session::move_storage, the file paths with
        // Session::rename_file.
        sqlx::query(
            "UPDATE torrents SET only_files = $1, is_paused = $2, output_folder = $3, renamed_files = $4, move_in_progress = $5 WHERE id = $6",
        )
        .bind(torrent.only_files().map(|v| {
            v.into_iter()
                .filter_map(|f| f.try_into().ok())
                .collect::<Vec<i32>>()
        }))
        .bind(torrent.is_paused())
        .bind(
            torrent
                .output_folder()
                .to_str()
                .context("output_folder")?
                .to_owned(),
        )
        .bind(renamed_files_column(torrent)?)
        .bind(move_in_progress_column(torrent)?)
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

// These tests need a Postgres server: they run against RQBIT_TEST_POSTGRES_URL and are skipped
// (with a note) when it isn't set. Each test works in a schema of its own, so they can run in
// parallel against one database.
#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use futures::TryStreamExt;
    use sqlx::{AssertSqlSafe, PgPool};
    use tempfile::TempDir;

    use super::*;
    use crate::{
        AddTorrent, AddTorrentOptions, CreateTorrentOptions, MoveStorageMode, Session,
        SessionOptions, create_torrent, spawn_utils::BlockingSpawner,
        tests::test_util::create_default_random_dir_with_torrents, torrent_state::MoveInProgress,
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

    // A paused torrent of two files, stored in the folder it was created from.
    struct Fixture {
        _dir: TempDir,
        data: PathBuf,
        session: Arc<Session>,
        handle: ManagedTorrentHandle,
    }

    async fn fixture(name: &str) -> anyhow::Result<Fixture> {
        let data = create_default_random_dir_with_torrents(2, 4096, Some(name));
        let torrent = create_torrent(
            data.path(),
            CreateTorrentOptions {
                piece_length: Some(1024),
                ..Default::default()
            },
            &BlockingSpawner::new(1),
        )
        .await?;
        let session = Session::new_with_opts(
            data.path().to_owned(),
            SessionOptions {
                dht: None,
                persistence: None,
                listen: None,
                ..Default::default()
            },
        )
        .await?;
        let handle = session
            .add_torrent(
                AddTorrent::from_bytes(torrent.as_bytes()?),
                Some(AddTorrentOptions {
                    paused: true,
                    overwrite: true,
                    output_folder: Some(data.path().to_str().context("utf-8")?.to_owned()),
                    ..Default::default()
                }),
            )
            .await?
            .into_handle()
            .context("into_handle")?;
        handle.wait_until_initialized().await?;
        Ok(Fixture {
            data: data.path().to_owned(),
            _dir: data,
            session,
            handle,
        })
    }

    fn file_name(handle: &ManagedTorrentHandle, index: usize) -> anyhow::Result<PathBuf> {
        handle.with_metadata(|m| m.file_infos[index].relative_filename.clone())
    }

    async fn load_all(
        store: &PostgresSessionStorage,
    ) -> anyhow::Result<Vec<(TorrentId, SerializedTorrent)>> {
        store.stream_all().await?.try_collect().await
    }

    // A move and a rename reach the row through update_metadata, and a torrent stored after
    // them is written with them; both read back the same through get and stream_all.
    #[tokio::test]
    async fn test_moved_and_renamed_round_trip() -> anyhow::Result<()> {
        let Some(scratch) = Scratch::new("moved_renamed").await? else {
            return Ok(());
        };
        let store = scratch.store().await?;
        let f = fixture("rqbit_pg_moved_renamed").await?;
        let id = store.next_id().await?;
        store.store(id, &f.handle).await?;
        assert_eq!(store.get(id).await?.output_folder, f.data);
        assert!(store.get(id).await?.renamed_files.is_empty());

        let moved = f.data.join("moved");
        f.session
            .move_storage(&f.handle, moved.clone(), MoveStorageMode::FailIfExists)
            .await?;
        f.session
            .rename_file(&f.handle, 1, PathBuf::from("nested/renamed.data"))
            .await?;
        assert!(moved.join("nested/renamed.data").exists());
        store.update_metadata(id, &f.handle).await?;

        let expected_renames = BTreeMap::from([(1, PathBuf::from("nested/renamed.data"))]);
        let st = store.get(id).await?;
        assert_eq!(st.output_folder, moved);
        assert_eq!(st.renamed_files, expected_renames);
        assert!(st.move_in_progress.is_none());
        assert_eq!(st.info_hash, f.handle.info_hash());

        // A fresh insert carries the same location.
        let id2 = store.next_id().await?;
        store.store(id2, &f.handle).await?;
        let all = load_all(&store).await?;
        assert_eq!(all.len(), 2);
        for (_, st) in &all {
            assert_eq!(st.output_folder, moved);
            assert_eq!(st.renamed_files, expected_renames);
        }

        // Renaming back to the torrent's own name empties the column again.
        let original = file_name(&f.handle, 1)?;
        f.session.rename_file(&f.handle, 1, original).await?;
        store.update_metadata(id, &f.handle).await?;
        assert!(store.get(id).await?.renamed_files.is_empty());

        scratch.drop_schema().await
    }

    // The marker of a move in progress is written, read back exactly and cleared.
    #[tokio::test]
    async fn test_move_in_progress_round_trip() -> anyhow::Result<()> {
        let Some(scratch) = Scratch::new("move_marker").await? else {
            return Ok(());
        };
        let store = scratch.store().await?;
        let f = fixture("rqbit_pg_move_marker").await?;
        let id = store.next_id().await?;
        store.store(id, &f.handle).await?;
        assert!(store.get(id).await?.move_in_progress.is_none());

        let marker = MoveInProgress {
            to: f.data.join("elsewhere"),
            files: vec![file_name(&f.handle, 0)?, file_name(&f.handle, 1)?],
        };
        f.handle.shared.location.write().move_in_progress = Some(marker.clone());
        store.update_metadata(id, &f.handle).await?;
        let st = store.get(id).await?;
        assert_eq!(st.move_in_progress.as_ref(), Some(&marker));
        // The folder is still the one moved from: that is what recovery starts from.
        assert_eq!(st.output_folder, f.data);
        assert_eq!(
            st.interrupted_move(),
            Some(
                marker
                    .files
                    .iter()
                    .map(|rel| (f.data.join(rel), marker.to.join(rel)))
                    .collect()
            )
        );
        // A row inserted while the marker is set has it as well.
        let id2 = store.next_id().await?;
        store.store(id2, &f.handle).await?;
        assert_eq!(store.get(id2).await?.move_in_progress, Some(marker));

        f.handle.shared.location.write().move_in_progress = None;
        store.update_metadata(id, &f.handle).await?;
        assert!(store.get(id).await?.move_in_progress.is_none());
        assert!(store.get(id).await?.interrupted_move().is_none());

        scratch.drop_schema().await
    }

    // A table created before renames and the move marker existed: the store
    // adds the columns and loads the old row with their defaults, and can update it.
    #[tokio::test]
    async fn test_table_from_before_storage_ops() -> anyhow::Result<()> {
        let Some(scratch) = Scratch::new("legacy").await? else {
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
        ] {
            sqlx::query(q).execute(&legacy).await?;
        }
        let info_hash = [7u8; 20];
        sqlx::query(
            "INSERT INTO torrents (id, info_hash, torrent_bytes, trackers, output_folder, only_files, is_paused)
            VALUES (5, $1, $2, $3, '/old/folder', $4, TRUE)",
        )
        .bind(&info_hash[..])
        .bind(&b"torrent"[..])
        .bind(vec!["udp://tracker.example:80".to_owned()])
        .bind(vec![0i32, 2])
        .execute(&legacy)
        .await?;
        legacy.close().await;

        let store = scratch.store().await?;
        let check = |st: &SerializedTorrent| {
            assert_eq!(st.info_hash, Id20::new(info_hash));
            assert_eq!(&st.torrent_bytes[..], b"torrent");
            assert_eq!(st.output_folder, Path::new("/old/folder"));
            assert_eq!(st.only_files, Some(vec![0, 2]));
            assert!(st.is_paused);
            assert!(st.trackers.contains("udp://tracker.example:80"));
            assert!(st.renamed_files.is_empty());
            assert!(st.move_in_progress.is_none());
        };
        check(&store.get(5).await?);
        let all = load_all(&store).await?;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, 5);
        check(&all[0].1);

        // Opening the store again (the columns now exist) changes nothing.
        drop(store);
        let store = scratch.store().await?;
        check(&store.get(5).await?);

        scratch.drop_schema().await
    }
}
