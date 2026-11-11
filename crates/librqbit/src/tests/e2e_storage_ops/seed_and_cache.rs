//! Seed mode against the fastresume bitfield and storages that can't tell file lengths, and
//! rechecks through the write-through cache.

use super::*;

// Adds `content` from `data` to a session with JSON persistence in `db`, waits for its check
// and stops the session: what stays behind is the fastresume bitfield of that check.
async fn leave_bitfield(
    content: &Content,
    dir: &Path,
    data: &Path,
    db: &Path,
) -> anyhow::Result<()> {
    let session = new_session(
        dir,
        SessionOpts {
            persistence: Some(db.to_owned()),
            ..Default::default()
        },
    )
    .await?;
    let handle = add(
        &session,
        content,
        data,
        AddTorrentOptions {
            paused: true,
            ..Default::default()
        },
    )
    .await?;
    session.stop().await;
    drop(handle);
    drop(session);
    Ok(())
}

// Copies the `.bitv` file of `from` into `to`, so that a session persisting into `to` finds
// the bitfield when the torrent is added to it (seed mode is an add option, not something a
// restored torrent remembers).
fn carry_bitfield(from: &Path, to: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let p = e?.path();
        if p.extension().is_some_and(|e| e == "bitv") {
            std::fs::copy(&p, to.join(p.file_name().context("no file name")?))?;
            return Ok(());
        }
    }
    bail!("no .bitv file in {from:?}")
}

// Adds `content` in seed mode, paused, to a session persisting into `db`, through a storage
// that counts what it reads.
async fn add_seed_mode(
    content: &Content,
    dir: &Path,
    data: &Path,
    db: &Path,
) -> anyhow::Result<(Arc<Session>, ManagedTorrentHandle, Arc<Ctl>)> {
    let session = new_session(
        dir,
        SessionOpts {
            persistence: Some(db.to_owned()),
            ..Default::default()
        },
    )
    .await?;
    let ctl = Arc::new(Ctl::default());
    let handle = add(
        &session,
        content,
        data,
        AddTorrentOptions {
            paused: true,
            skip_initial_check: true,
            storage_factory: Some(InstrumentedFactory { ctl: ctl.clone() }.boxed()),
            ..Default::default()
        },
    )
    .await?;
    Ok((session, handle, ctl))
}

// A valid bitfield wins over seed mode: the file that was missing when it was stored is on
// disk now with its right length (and bytes), yet the torrent still has only what the
// bitfield says, having read no more than the pieces it samples to validate it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_seed_mode_keeps_a_valid_partial_bitfield() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(SIX_PIECES, false);
        let dir = TempDir::with_prefix("rqbit_seed_mode_valid_bitfield")?;
        let data = dir.path().join("data");
        let first_db = dir.path().join("db1");
        let second_db = dir.path().join("db2");
        content.write_to(&data);
        std::fs::remove_file(data.join("two.bin"))?;
        let partial = content.pieces_of(&["one.bin", "three.bin"]);

        leave_bitfield(&content, dir.path(), &data, &first_db).await?;
        assert_eq!(stored_bitfield(&first_db)?, partial);

        std::fs::write(data.join("two.bin"), &content.file("two.bin").data)?;
        carry_bitfield(&first_db, &second_db)?;
        let (_session, handle, ctl) =
            add_seed_mode(&content, dir.path(), &data, &second_db).await?;

        assert_eq!(have_pieces(&handle)?, partial);
        assert!(!handle.stats().finished);
        let read = ctl.read_bytes.load(Ordering::SeqCst);
        assert!(read > 0, "the bitfield wasn't validated");
        assert!(
            read <= (partial.len() * PIECE_LEN) as u64,
            "read {read} bytes: more than the pieces the bitfield claims, so the data was hashed"
        );
        assert_eq!(stored_bitfield(&second_db)?, partial);
        Ok(())
    })
    .await
}

// A stored bitfield that the data contradicts says the data changed after it was written.
// Lengths can't tell what changed, so seed mode hashes everything instead of trusting them,
// and the damaged pieces are missing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_seed_mode_with_a_broken_bitfield_checks_everything() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(SIX_PIECES, false);
        let dir = TempDir::with_prefix("rqbit_seed_mode_broken_bitfield")?;
        let data = dir.path().join("data");
        let first_db = dir.path().join("db1");
        let second_db = dir.path().join("db2");
        content.write_to(&data);

        leave_bitfield(&content, dir.path(), &data, &first_db).await?;
        assert_eq!(stored_bitfield(&first_db)?, content.all_pieces());

        // The bitfield claims piece 0, the first piece of its file, which its validation
        // always hashes: the validation fails whatever it samples besides.
        content.corrupt(&data, 0);
        content.corrupt(&data, 3);
        carry_bitfield(&first_db, &second_db)?;
        let (_session, handle, ctl) =
            add_seed_mode(&content, dir.path(), &data, &second_db).await?;

        let expected = without(content.all_pieces(), &[0, 3]);
        assert_eq!(have_pieces(&handle)?, expected);
        let read = ctl.read_bytes.load(Ordering::SeqCst);
        assert!(
            read >= content.total_len as u64,
            "read {read} bytes, less than the content: it wasn't hashed"
        );
        assert_eq!(stored_bitfield(&second_db)?, expected);
        Ok(())
    })
    .await
}

// A storage that can't tell the length of its files: `file_length` keeps the default answer.
#[derive(Clone, Default)]
struct NoLengthsFactory {
    inner: InstrumentedFactory,
}

struct NoLengths {
    inner: Box<dyn TorrentStorage>,
}

impl StorageFactory for NoLengthsFactory {
    type Storage = NoLengths;

    fn create(
        &self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<NoLengths> {
        Ok(NoLengths {
            inner: Box::new(self.inner.create(shared, metadata)?),
        })
    }

    fn clone_box(&self) -> BoxStorageFactory {
        self.clone().boxed()
    }
}

impl TorrentStorage for NoLengths {
    fn init(
        &mut self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        self.inner.init(shared, metadata)
    }

    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        self.inner.pread_exact(file_id, offset, buf)
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        self.inner.pwrite_all(file_id, offset, buf)
    }

    fn remove_file(&self, file_id: usize, filename: &Path) -> anyhow::Result<()> {
        self.inner.remove_file(file_id, filename)
    }

    fn remove_directory_if_empty(&self, path: &Path) -> anyhow::Result<()> {
        self.inner.remove_directory_if_empty(path)
    }

    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()> {
        self.inner.ensure_file_length(file_id, length)
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        Ok(Box::new(NoLengths {
            inner: self.inner.take()?,
        }))
    }
}

// Seed mode needs the storage to tell file lengths. One that can't gets a full check, which
// finds exactly the damaged piece, although every file has its right length.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_seed_mode_without_file_lengths_checks_everything() -> anyhow::Result<()> {
    run(async {
        let content = Content::new(SIX_PIECES, false);
        let dir = TempDir::with_prefix("rqbit_seed_mode_no_lengths")?;
        content.write_to(dir.path());
        content.corrupt(dir.path(), 3);
        let session = new_session(dir.path(), Default::default()).await?;
        let factory = NoLengthsFactory::default();
        let ctl = factory.inner.ctl.clone();
        let handle = add(
            &session,
            &content,
            dir.path(),
            AddTorrentOptions {
                paused: true,
                skip_initial_check: true,
                storage_factory: Some(factory.boxed()),
                ..Default::default()
            },
        )
        .await?;

        assert_eq!(have_pieces(&handle)?, without(content.all_pieces(), &[3]));
        let read = ctl.read_bytes.load(Ordering::SeqCst);
        assert!(
            read >= content.total_len as u64,
            "read {read} bytes, less than the content: it wasn't hashed"
        );
        Ok(())
    })
    .await
}
