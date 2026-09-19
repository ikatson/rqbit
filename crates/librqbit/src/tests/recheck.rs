use std::{
    net::Ipv4Addr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, bail};
use librqbit_core::Id20;

use crate::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, CreateTorrentOptions, ManagedTorrentState,
    Session, SessionOptions, SessionPersistenceConfig, create_torrent,
    spawn_utils::BlockingSpawner,
    tests::test_util::{create_default_random_dir_with_torrents, setup_test_logging, wait_until},
    torrent_state::ManagedTorrentHandle,
};

const PIECE_LENGTH: u32 = 16384 * 2;
const FILE_LENGTH: usize = 8 * 1000 * 1000;
const NUM_FILES: usize = 2;

struct Fixture {
    // Data dir with the original files. Files are named "{i}.data".
    data_dir: PathBuf,
    // Persistence dir for the JsonSessionPersistenceStore.
    persistence_dir: PathBuf,
    torrent_bytes: Vec<u8>,
    info_hash: Id20,
    total_length: u64,
    piece_length: u32,
}

impl Fixture {
    fn file_path(&self, idx: usize) -> PathBuf {
        self.data_dir.join(format!("{idx}.data"))
    }

    fn bitv_filename(&self) -> PathBuf {
        self.persistence_dir
            .join(format!("{:?}", self.info_hash))
            .with_extension("bitv")
    }
}

// Creates files with random content, a torrent out of them, and the persistence layout.
// The torrent file paths are "{i}.data" relative to the torrent name, and the session is
// expected to be used with output_folder = data_dir.
async fn make_fixture(prefix: &str) -> Fixture {
    let tempdir = create_default_random_dir_with_torrents(NUM_FILES, FILE_LENGTH, Some(prefix));
    let data_dir = tempdir.keep();
    let torrent = create_torrent(
        &data_dir,
        CreateTorrentOptions {
            piece_length: Some(PIECE_LENGTH),
            ..Default::default()
        },
        &BlockingSpawner::new(1),
    )
    .await
    .unwrap();
    let torrent_bytes = torrent.as_bytes().unwrap().to_vec();
    let info_hash = librqbit_core::torrent_metainfo::torrent_from_bytes(&torrent_bytes)
        .unwrap()
        .info_hash;
    let persistence_dir = data_dir.with_file_name(format!(
        "{}_session",
        data_dir.file_name().unwrap().to_str().unwrap()
    ));
    std::fs::create_dir_all(&persistence_dir).unwrap();
    Fixture {
        data_dir,
        persistence_dir,
        torrent_bytes,
        info_hash,
        total_length: (FILE_LENGTH * NUM_FILES) as u64,
        piece_length: PIECE_LENGTH,
    }
}

fn session_opts(persistence_dir: &Path) -> SessionOptions {
    SessionOptions {
        dht: None,
        persistence: Some(SessionPersistenceConfig::Json {
            folder: Some(persistence_dir.to_owned()),
        }),
        fastresume: true,
        listen: Some(crate::ListenerOptions {
            mode: crate::ListenerMode::TcpOnly,
            listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
            ..Default::default()
        }),
        disable_local_service_discovery: true,
        ..Default::default()
    }
}

async fn new_session(persistence_dir: &Path) -> Arc<Session> {
    Session::new_with_opts(persistence_dir.to_owned(), session_opts(persistence_dir))
        .await
        .unwrap()
}

async fn add(
    session: &Arc<Session>,
    fixture: &Fixture,
    opts: AddTorrentOptions,
) -> anyhow::Result<ManagedTorrentHandle> {
    let r = session
        .add_torrent(
            AddTorrent::TorrentFileBytes(fixture.torrent_bytes.clone().into()),
            Some(opts),
        )
        .await
        .context("add_torrent")?;
    match r {
        AddTorrentResponse::Added(_, handle) => Ok(handle),
        AddTorrentResponse::AlreadyManaged(_, handle) => Ok(handle),
        AddTorrentResponse::ListOnly(_) => bail!("unexpected list_only"),
    }
}

fn add_opts(output_folder: &Path, check_after_load: bool) -> AddTorrentOptions {
    AddTorrentOptions {
        paused: true,
        overwrite: true,
        output_folder: Some(output_folder.to_str().unwrap().to_owned()),
        check_after_load,
        ..Default::default()
    }
}

/// Waits until the torrent is out of the initializing state, and asserts it's paused
/// with the given "finished" flag.
async fn wait_paused_finished(handle: &ManagedTorrentHandle, finished: bool) {
    wait_until(
        || {
            handle.with_state(|s| match s {
                ManagedTorrentState::Paused(p) => {
                    if p.hns().finished() != finished {
                        bail!("expected finished={finished}, got {}", p.hns().finished())
                    }
                    Ok(())
                }
                ManagedTorrentState::Error(e) => bail!("torrent errored: {e:#}"),
                ManagedTorrentState::Initializing(_) => bail!("still initializing"),
                other => bail!("unexpected state {}", other.name()),
            })
        },
        Duration::from_secs(30),
    )
    .await
    .unwrap();
}

async fn wait_live(handle: &ManagedTorrentHandle) {
    wait_until(
        || {
            handle.with_state(|s| match s {
                ManagedTorrentState::Live(_) => Ok(()),
                ManagedTorrentState::Error(e) => bail!("torrent errored: {e:#}"),
                other => bail!("unexpected state {}", other.name()),
            })
        },
        Duration::from_secs(30),
    )
    .await
    .unwrap();
}

async fn wait_live_finished(handle: &ManagedTorrentHandle, finished: bool) {
    wait_until(
        || {
            handle.with_state(|s| match s {
                ManagedTorrentState::Live(l) => match l.get_hns() {
                    Some(hns) => {
                        if hns.finished() != finished {
                            bail!("expected finished={finished}, got {}", hns.finished())
                        }
                        Ok(())
                    }
                    None => bail!("no hns yet"),
                },
                ManagedTorrentState::Error(e) => bail!("torrent errored: {e:#}"),
                other => bail!("unexpected state {}", other.name()),
            })
        },
        Duration::from_secs(30),
    )
    .await
    .unwrap();
}

// Corrupts one byte in the beginning of file 0. With the piece length above, exactly
// one piece (piece 0) loses integrity.
fn corrupt_one_byte(fixture: &Fixture) {
    let path = fixture.file_path(0);
    let contents = std::fs::read(&path).unwrap();
    assert!(contents.len() as u64 >= fixture.piece_length as u64);
    let mut contents = contents;
    contents[0] ^= 0xff;
    std::fs::write(&path, contents).unwrap();
}

fn restore_file(fixture: &Fixture, idx: usize, contents: &[u8]) {
    std::fs::write(fixture.file_path(idx), contents).unwrap();
}

fn read_file(fixture: &Fixture, idx: usize) -> Vec<u8> {
    std::fs::read(fixture.file_path(idx)).unwrap()
}

// Removes the session.json of the fixture, so the next session doesn't restore the
// torrents from the previous one. The .bitv files are kept: they are what we test.
// Without this, add_torrent() would return AlreadyManaged with a torrent restored
// using the default options.
fn clear_session_db(fixture: &Fixture) {
    let db = fixture.persistence_dir.join("session.json");
    if db.exists() {
        std::fs::remove_file(&db).unwrap();
    }
}

// (have_bytes, finished) as seen from the paused state.
fn paused_have_finished(handle: &ManagedTorrentHandle) -> (u64, bool) {
    handle
        .with_state(|s| match s {
            ManagedTorrentState::Paused(p) => {
                let hns = p.hns();
                Ok((hns.have_bytes, hns.finished()))
            }
            other => bail!("unexpected state {}", other.name()),
        })
        .unwrap()
}

fn live_have_finished(handle: &ManagedTorrentHandle) -> (u64, bool) {
    handle
        .with_state(|s| match s {
            ManagedTorrentState::Live(l) => {
                let hns = l.get_hns().unwrap();
                Ok((hns.have_bytes, hns.finished()))
            }
            other => bail!("unexpected state {}", other.name()),
        })
        .unwrap()
}

// One piece (piece 0 of file 0) should be lost after corrupting a single byte.
fn expected_have_after_corruption(fixture: &Fixture) -> u64 {
    fixture.total_length - fixture.piece_length as u64
}

/// The default path: with no persisted bitfield, a full check runs and reflects the
/// truth; with a persisted bitfield, the fastresume spot-check catches the corruption.
#[tokio::test(flavor = "multi_thread")]
async fn test_check_after_load_default_regression() {
    setup_test_logging();
    let fixture = make_fixture("check_after_load_default").await;

    // Session 1: verify and persist the bitfield.
    let session = new_session(&fixture.persistence_dir).await;
    let handle = add(&session, &fixture, add_opts(&fixture.data_dir, true))
        .await
        .unwrap();
    wait_paused_finished(&handle, true).await;
    drop(session);
    drop(handle);
    clear_session_db(&fixture);

    // Corrupt the data, then make sure the default path still catches it:
    // the spot-check validates at least the first piece of each file.
    corrupt_one_byte(&fixture);
    let session = new_session(&fixture.persistence_dir).await;
    let handle = add(&session, &fixture, add_opts(&fixture.data_dir, true))
        .await
        .unwrap();
    wait_paused_finished(&handle, false).await;
    let (have, finished) = paused_have_finished(&handle);
    assert!(!finished);
    assert_eq!(have, expected_have_after_corruption(&fixture));
}

/// check_after_load=false with an existing (right length) bitfield: no checks run, the
/// torrent is trusted as-is, i.e. even corrupted data is reported as 100% have.
#[tokio::test(flavor = "multi_thread")]
async fn test_check_after_load_false_trusted_bitfield() {
    setup_test_logging();
    let fixture = make_fixture("check_after_load_false_trusted").await;

    // Session 1: verify and persist the bitfield.
    let session = new_session(&fixture.persistence_dir).await;
    let handle = add(&session, &fixture, add_opts(&fixture.data_dir, true))
        .await
        .unwrap();
    wait_paused_finished(&handle, true).await;
    drop(session);
    drop(handle);
    clear_session_db(&fixture);

    // Session 2: corrupt the data and add with check_after_load=false.
    // The spot-check would catch this corruption, so reaching 100% proves no check ran.
    corrupt_one_byte(&fixture);
    let session = new_session(&fixture.persistence_dir).await;
    let handle = add(&session, &fixture, add_opts(&fixture.data_dir, false))
        .await
        .unwrap();
    wait_paused_finished(&handle, true).await;
    let (have, finished) = paused_have_finished(&handle);
    assert!(finished);
    assert_eq!(have, fixture.total_length);
}

/// check_after_load=false with no persisted bitfield: MUST fall back to a full check.
#[tokio::test(flavor = "multi_thread")]
async fn test_check_after_load_false_fallback_no_bitfield() {
    setup_test_logging();
    let fixture = make_fixture("check_after_load_false_fallback").await;
    corrupt_one_byte(&fixture);

    // No bitfield was ever persisted: the safety contract requires a full check.
    let session = new_session(&fixture.persistence_dir).await;
    let handle = add(&session, &fixture, add_opts(&fixture.data_dir, false))
        .await
        .unwrap();
    wait_paused_finished(&handle, false).await;
    let (have, finished) = paused_have_finished(&handle);
    assert!(!finished);
    assert_eq!(have, expected_have_after_corruption(&fixture));
}

/// check_after_load=false with a wrong-length persisted bitfield: MUST fall back to a
/// full check (never silently trust it).
#[tokio::test(flavor = "multi_thread")]
async fn test_check_after_load_false_fallback_wrong_length() {
    setup_test_logging();
    let fixture = make_fixture("check_after_load_false_wronglen").await;
    corrupt_one_byte(&fixture);

    // Fabricate a wrong-length persisted bitfield.
    let bitv = fixture.bitv_filename();
    std::fs::write(&bitv, [0u8; 7]).unwrap();

    let session = new_session(&fixture.persistence_dir).await;
    let handle = add(&session, &fixture, add_opts(&fixture.data_dir, false))
        .await
        .unwrap();
    wait_paused_finished(&handle, false).await;
    let (have, finished) = paused_have_finished(&handle);
    assert!(!finished);
    assert_eq!(have, expected_have_after_corruption(&fixture));
}

/// Recheck of a live torrent: detects corruption, stays live, have reflects the truth;
/// restoring the data and rechecking again restores the 100% have.
#[tokio::test(flavor = "multi_thread")]
async fn test_recheck_detects_corruption_and_recovers_live() {
    setup_test_logging();
    let fixture = make_fixture("recheck_corrupt_live").await;

    let session = new_session(&fixture.persistence_dir).await;
    let handle = add(&session, &fixture, add_opts(&fixture.data_dir, true))
        .await
        .unwrap();
    wait_paused_finished(&handle, true).await;

    // Make it live first.
    session.unpause(&handle).await.unwrap();
    wait_live(&handle).await;

    // Corrupt the data and recheck. Torrent should be live again, but no longer finished.
    let file0 = read_file(&fixture, 0);
    corrupt_one_byte(&fixture);
    handle.recheck().unwrap();
    handle.wait_until_initialized().await.unwrap();
    wait_live_finished(&handle, false).await;
    {
        let (have, finished) = live_have_finished(&handle);
        assert!(!finished);
        assert_eq!(have, expected_have_after_corruption(&fixture));
    }

    // Restore the data and recheck again. Torrent should be live and finished.
    restore_file(&fixture, 0, &file0);
    handle.recheck().unwrap();
    handle.wait_until_initialized().await.unwrap();
    wait_live_finished(&handle, true).await;
    {
        let (have, finished) = live_have_finished(&handle);
        assert!(finished);
        assert_eq!(have, fixture.total_length);
    }
}

/// Recheck of a paused torrent: stays paused, updates have.
#[tokio::test(flavor = "multi_thread")]
async fn test_recheck_paused_stays_paused() {
    setup_test_logging();
    let fixture = make_fixture("recheck_paused").await;

    let session = new_session(&fixture.persistence_dir).await;
    let handle = add(&session, &fixture, add_opts(&fixture.data_dir, true))
        .await
        .unwrap();
    wait_paused_finished(&handle, true).await;
    assert!(handle.is_paused());

    corrupt_one_byte(&fixture);
    handle.recheck().unwrap();
    handle.wait_until_initialized().await.unwrap();
    wait_paused_finished(&handle, false).await;
    assert!(handle.is_paused());
    let (have, finished) = paused_have_finished(&handle);
    assert!(!finished);
    assert_eq!(have, expected_have_after_corruption(&fixture));
}

/// Recheck of a partial download (unselected file): selected file's truth is preserved,
/// and the unselected file's corruption is reflected in have_pieces.
#[tokio::test(flavor = "multi_thread")]
async fn test_recheck_partial_preserves_truth() {
    setup_test_logging();
    let fixture = make_fixture("recheck_partial").await;

    let parsed =
        librqbit_core::torrent_metainfo::torrent_from_bytes(&fixture.torrent_bytes).unwrap();
    let raw_info = parsed.info.data.clone();
    eprintln!("raw piece_length={}", raw_info.piece_length);
    if let Some(files) = &raw_info.files {
        for (idx, f) in files.iter().enumerate() {
            eprintln!("raw file {idx}: {:?} len={}", f.path, f.length);
        }
    }
    let info = parsed.info.data.validate().unwrap();
    eprintln!("total_pieces={:?}", info.lengths().total_pieces());
    use librqbit_core::lengths::{Lengths, last_element_size};
    eprintln!(
        "direct last_element_size={}",
        last_element_size(16_000_000u64, 32768u64)
    );
    let direct = Lengths::new(16_000_000u64, 32768u32).unwrap();
    eprintln!(
        "direct new last_piece_length={}",
        direct.piece_length(direct.last_piece_id())
    );
    let from_torrent = Lengths::from_torrent(&raw_info).unwrap();
    eprintln!("from_torrent total={}", from_torrent.total_length());
    eprintln!(
        "from_torrent last_piece_length={}",
        from_torrent.piece_length(from_torrent.last_piece_id())
    );
    eprintln!("from_torrent total_pieces={}", from_torrent.total_pieces());
    eprintln!("total_length={:?}", info.lengths().total_length());
    for (idx, fd) in info.iter_file_details_ext().enumerate() {
        eprintln!(
            "file {idx}: name={:?} len={} offset={} pieces={:?}",
            fd.details.filename, fd.details.len, fd.offset, fd.pieces
        );
    }
    for idx in 0..2 {
        eprintln!(
            "ondisk {idx}: len={}",
            std::fs::metadata(fixture.file_path(idx)).unwrap().len()
        );
    }

    let session = new_session(&fixture.persistence_dir).await;
    let handle = add(
        &session,
        &fixture,
        AddTorrentOptions {
            paused: true,
            overwrite: true,
            output_folder: Some(fixture.data_dir.to_str().unwrap().to_owned()),
            only_files: Some(vec![0]),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    wait_paused_finished(&handle, true).await;

    // Corrupt only the unselected file's last byte. The selected file is intact, so the
    // torrent remains "finished" (all selected pieces present), but have_bytes drops by
    // the size of the last (truncated) piece, which only contains file 1's data.
    corrupt_file(&fixture, 1);
    handle.recheck().unwrap();
    handle.wait_until_initialized().await.unwrap();
    wait_paused_finished(&handle, true).await;
    let (have, finished) = paused_have_finished(&handle);
    assert!(finished);
    assert_eq!(have, fixture.total_length - last_piece_len(&fixture));
}

// The length of the last (possibly truncated) piece.
fn last_piece_len(fixture: &Fixture) -> u64 {
    fixture.total_length % fixture.piece_length as u64
}

// Corrupts the last byte of the file: with the piece length above this only affects
// the last piece, which (unlike the first byte) doesn't cross into the previous file.
fn corrupt_file(fixture: &Fixture, idx: usize) {
    let path = fixture.file_path(idx);
    let mut contents = std::fs::read(&path).unwrap();
    let last = contents.len() - 1;
    contents[last] ^= 0xff;
    std::fs::write(&path, contents).unwrap();
}

/// Double recheck and recheck-during-initialization are rejected with a clear error.
#[tokio::test(flavor = "multi_thread")]
async fn test_recheck_rejections() {
    setup_test_logging();
    let fixture = make_fixture("recheck_rejections").await;

    let session = new_session(&fixture.persistence_dir).await;
    let handle = add(&session, &fixture, add_opts(&fixture.data_dir, true))
        .await
        .unwrap();
    wait_paused_finished(&handle, true).await;
    session.unpause(&handle).await.unwrap();
    wait_live(&handle).await;

    // Start a recheck, then request pause so that the check stops and the torrent
    // stays in the initializing state. This makes the rejection deterministic.
    handle.recheck().unwrap();
    session.pause(&handle).await.unwrap();

    // While initializing, a second recheck must be rejected.
    let err = match handle.recheck() {
        Err(e) => e,
        Ok(_) => panic!("second recheck should fail"),
    };
    assert!(
        err.to_string().contains("initializing"),
        "unexpected error: {err:#}"
    );

    // Recheck of a live torrent, paused mid-check, resumed via unpause: the check
    // re-runs and the torrent goes live again.
    session.unpause(&handle).await.unwrap();
    handle.wait_until_initialized().await.unwrap();
    wait_live_finished(&handle, true).await;
}

/// Two concurrent rechecks don't corrupt the state machine: at least one runs, the
/// other is rejected or runs after, and the torrent ends up consistent.
#[tokio::test(flavor = "multi_thread")]
async fn test_recheck_concurrent_is_safe() {
    setup_test_logging();
    let fixture = make_fixture("recheck_concurrent").await;

    let session = new_session(&fixture.persistence_dir).await;
    let handle = add(&session, &fixture, add_opts(&fixture.data_dir, true))
        .await
        .unwrap();
    wait_paused_finished(&handle, true).await;
    session.unpause(&handle).await.unwrap();
    wait_live(&handle).await;

    corrupt_one_byte(&fixture);

    let h1 = handle.clone();
    let h2 = handle.clone();
    let (r1, r2) = tokio::join!(
        tokio::spawn(async move { h1.recheck() }),
        tokio::spawn(async move { h2.recheck() }),
    );
    let r1 = r1.unwrap();
    let r2 = r2.unwrap();
    assert!(
        r1.is_ok() || r2.is_ok(),
        "at least one recheck should succeed: {r1:?}, {r2:?}"
    );

    handle.wait_until_initialized().await.unwrap();
    wait_live_finished(&handle, false).await;
    let (have, _) = live_have_finished(&handle);
    assert_eq!(have, expected_have_after_corruption(&fixture));
}

/// Full sweeper story: recheck persists the updated bitfield, so the next boot with
/// check_after_load=false trusts the new (truthful) bitfield.
#[tokio::test(flavor = "multi_thread")]
async fn test_recheck_persists_updated_bitfield() {
    setup_test_logging();
    let fixture = make_fixture("recheck_persists").await;

    // Session 1: verify and persist.
    let session = new_session(&fixture.persistence_dir).await;
    let handle = add(&session, &fixture, add_opts(&fixture.data_dir, true))
        .await
        .unwrap();
    wait_paused_finished(&handle, true).await;
    drop(session);
    drop(handle);
    clear_session_db(&fixture);

    // Corrupt, then boot with check_after_load=false: stale bitfield is trusted
    // (by design), and the persisted bitfield still says 100%.
    corrupt_one_byte(&fixture);
    let session = new_session(&fixture.persistence_dir).await;
    let handle = add(&session, &fixture, add_opts(&fixture.data_dir, false))
        .await
        .unwrap();
    wait_paused_finished(&handle, true).await;
    drop(session);
    drop(handle);
    clear_session_db(&fixture);

    // Recheck on the same session-less state: add with check_after_load=false again,
    // then recheck. The recheck must persist the truthful bitfield.
    let session = new_session(&fixture.persistence_dir).await;
    let handle = add(&session, &fixture, add_opts(&fixture.data_dir, false))
        .await
        .unwrap();
    wait_paused_finished(&handle, true).await;
    handle.recheck().unwrap();
    handle.wait_until_initialized().await.unwrap();
    wait_paused_finished(&handle, false).await;
    drop(session);
    drop(handle);
    clear_session_db(&fixture);

    // New boot with check_after_load=false: the truthful bitfield is now trusted.
    let session = new_session(&fixture.persistence_dir).await;
    let handle = add(&session, &fixture, add_opts(&fixture.data_dir, false))
        .await
        .unwrap();
    wait_paused_finished(&handle, false).await;
    let (have, finished) = paused_have_finished(&handle);
    assert!(!finished);
    assert_eq!(have, expected_have_after_corruption(&fixture));
}
