//! Drives the deferred integrity checks scenario:
//!
//! - `timing`: creates N local torrents, and measures session boot times with the
//!   default check (full hash or fastresume spot-check) vs `check_after_load=false`.
//! - `live`: downloads a real torrent, then verifies that a restart with
//!   `check_after_load=false` skips the check and seeds instantly, that recheck()
//!   detects corrupted data, and that recheck() re-verifies restored data.
//!
//! Usage: integrity_scenario <timing|live> <torrent-file-or-magnet>
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use librqbit::{AddTorrent, AddTorrentOptions, Session, SessionOptions, SessionPersistenceConfig};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();
    let args: Vec<String> = std::env::args().collect();
    let what = args.get(1).context("expected timing|live")?.as_str();
    match what {
        "timing" => timing().await,
        "live" => {
            let src = args.get(2).context("expected a torrent file path")?;
            live(Path::new(src)).await
        }
        _ => bail!("expected timing|live"),
    }
}

// Removes session.json so the next session doesn't restore the torrents itself
// (restored torrents would use default options and skew the timings).
fn drop_db(session_dir: &Path) {
    let _ = std::fs::remove_file(session_dir.join("session.json"));
}

async fn new_session(
    outdir: &Path,
    persistence: Option<&Path>,
    fastresume: bool,
) -> anyhow::Result<Arc<Session>> {
    let opts = SessionOptions {
        persistence: persistence.map(|folder| SessionPersistenceConfig::Json {
            folder: Some(folder.to_owned()),
        }),
        fastresume,
        disable_local_service_discovery: true,
        ..Default::default()
    };
    Session::new_with_opts(outdir.to_owned(), opts)
        .await
        .context("error creating session")
}

async fn add_and_wait_checked(
    session: &Arc<Session>,
    src: &Path,
    output_folder: Option<String>,
    check_after_load: bool,
) -> anyhow::Result<Arc<librqbit::ManagedTorrent>> {
    let t0 = Instant::now();
    let handle = session
        .add_torrent(
            AddTorrent::TorrentFileBytes(std::fs::read(src)?.into()),
            Some(AddTorrentOptions {
                paused: true,
                overwrite: true,
                output_folder,
                check_after_load,
                ..Default::default()
            }),
        )
        .await
        .context("error adding torrent")?
        .into_handle()
        .context("expected handle")?;
    handle.wait_until_initialized().await.context("checking")?;
    let elapsed = t0.elapsed();
    let st = handle.stats();
    let finished = st.finished;
    let progress = st.progress_bytes;
    println!(
        "ADD-DONE src={src:?} check_after_load={check_after_load} elapsed_ms={} finished={finished} progress_bytes={progress}",
        elapsed.as_millis()
    );
    Ok(handle)
}

// Creates N torrents of `size_mb` each from random data, verifies them in a first
// session, then measures the boot (re-add) time with the default options vs
// check_after_load=false.
async fn timing() -> anyhow::Result<()> {
    const NUM: usize = 20;
    const SIZE_MB: usize = 64;

    let root = std::env::temp_dir().join(format!("integrity_timing_{}", std::process::id()));
    std::fs::create_dir_all(&root)?;
    let data_dir = root.join("data");
    std::fs::create_dir_all(&data_dir)?;
    let session_dir = root.join("session");

    println!(
        "creating {NUM} torrents of {SIZE_MB}MB each in {:?}",
        data_dir
    );
    let mut torrents = Vec::new();
    for i in 0..NUM {
        let dir = data_dir.join(format!("t{i}"));
        std::fs::create_dir_all(&dir)?;
        librqbit::spawn_utils::BlockingSpawner::new(1)
            .block_in_place(|| {
                use rand::{Rng, SeedableRng};
                use std::io::Write;
                let mut rng = rand::rngs::SmallRng::seed_from_u64(i as u64);
                let mut file = std::fs::File::create(dir.join("big.data"))?;
                let mut written = 0usize;
                let mut buf = vec![0u8; 1 << 20];
                while written < SIZE_MB << 20 {
                    rng.fill_bytes(&mut buf);
                    file.write_all(&buf)?;
                    written += buf.len();
                }
                Ok::<_, std::io::Error>(())
            })
            .context("error creating data")?;
        let created = librqbit::create_torrent(
            &dir,
            librqbit::CreateTorrentOptions::default(),
            &librqbit::spawn_utils::BlockingSpawner::new(1),
        )
        .await?;
        let bytes = created;
        let torrent_path = root.join(format!("t{i}.torrent"));
        std::fs::write(&torrent_path, bytes.as_bytes().unwrap())?;
        torrents.push(torrent_path);
    }

    // Phase 1: first boot: full checks (this is also where the bitfields get persisted).
    let session = new_session(&root, Some(&session_dir), true).await?;
    let t0 = Instant::now();
    for t in &torrents {
        add_and_wait_checked(
            &session,
            t,
            Some(root.join("data").to_string_lossy().into_owned()),
            true,
        )
        .await?;
    }
    println!(
        "TIMING: first_boot_full_check_total_ms={} per_torrent={}",
        t0.elapsed().as_millis(),
        t0.elapsed().as_millis() / NUM as u128
    );
    drop(session);

    // Phase 2: "before" #1 (the keep-at pain today): restart WITHOUT fastresume:
    // the full hash of everything runs on every boot.
    drop_db(&session_dir);
    let t0 = Instant::now();
    let session = new_session(&root, Some(&session_dir), false).await?;
    for t in &torrents {
        add_and_wait_checked(
            &session,
            t,
            Some(root.join("data").to_string_lossy().into_owned()),
            true,
        )
        .await?;
    }
    println!(
        "TIMING: boot_no_fastresume_total_ms={} per_torrent={}",
        t0.elapsed().as_millis(),
        t0.elapsed().as_millis() / NUM as u128
    );
    drop(session);

    // Phase 3: "before" #2: restart with fastresume: cheap spot-check.
    drop_db(&session_dir);
    let t0 = Instant::now();
    let session = new_session(&root, Some(&session_dir), true).await?;
    for t in &torrents {
        add_and_wait_checked(
            &session,
            t,
            Some(root.join("data").to_string_lossy().into_owned()),
            true,
        )
        .await?;
    }
    println!(
        "TIMING: boot_fastresume_spotcheck_total_ms={} per_torrent={}",
        t0.elapsed().as_millis(),
        t0.elapsed().as_millis() / NUM as u128
    );
    drop(session);

    // Phase 4: "after": restart with check_after_load=false: no checks at all.
    drop_db(&session_dir);
    let t0 = Instant::now();
    let session = new_session(&root, Some(&session_dir), true).await?;
    for t in &torrents {
        add_and_wait_checked(
            &session,
            t,
            Some(root.join("data").to_string_lossy().into_owned()),
            false,
        )
        .await?;
    }
    println!(
        "TIMING: boot_check_after_load_false_total_ms={} per_torrent={}",
        t0.elapsed().as_millis(),
        t0.elapsed().as_millis() / NUM as u128
    );
    drop(session);
    let _ = session_dir; // keep persistence files for inspection
    Ok(())
}

async fn live(src: &Path) -> anyhow::Result<()> {
    let existing_outdir: Option<String> = std::env::var("INTEGRITY_EXISTING_OUTDIR").ok();
    let root = std::env::temp_dir().join(format!("integrity_live_{}", std::process::id()));
    let outdir = existing_outdir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("out"));
    let session_dir = root.join("session");
    std::fs::create_dir_all(&outdir)?;
    std::fs::create_dir_all(&session_dir)?;

    // Boot 1: normal add (no persisted bitfield yet): with pre-existing data this is a
    // full check; otherwise it downloads the data for real.
    let session = new_session(&root, Some(&session_dir), true).await?;
    let handle = session
        .add_torrent(
            AddTorrent::TorrentFileBytes(std::fs::read(src)?.into()),
            Some(AddTorrentOptions {
                paused: false,
                overwrite: true,
                output_folder: Some(outdir.to_string_lossy().into_owned()),
                ..Default::default()
            }),
        )
        .await
        .context("error adding torrent")?
        .into_handle()
        .context("expected handle")?;
    let download_started = std::time::Instant::now();
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let st = handle.stats();
        if st.finished {
            break;
        }
        if st.state.to_string().contains("Error") {
            bail!("torrent errored: {:?}", st.error);
        }
        if download_started.elapsed() > Duration::from_secs(180) {
            bail!("timed out downloading/verifying, stats: {st}");
        }
    }
    println!(
        "LIVE-BOOT1: check_after_load=true elapsed_ms={} finished={}",
        download_started.elapsed().as_millis(),
        handle.stats().finished
    );
    let file_path = {
        let files = handle.metadata.load_full().unwrap().file_infos.clone();
        let fi = files.first().context("expected a file")?;
        outdir.join(&fi.relative_filename)
    };
    println!("LIVE: data file at {:?}", file_path);

    // Then stop the session (simulates a daemon restart).
    drop(handle);
    session.stop().await;

    // Boot 2: check_after_load=false: must not run any check.
    let boot2_started = Instant::now();
    let session = new_session(&root, Some(&session_dir), true).await?;
    let handle = session
        .add_torrent(
            AddTorrent::TorrentFileBytes(std::fs::read(src)?.into()),
            Some(AddTorrentOptions {
                paused: false,
                overwrite: true,
                output_folder: Some(outdir.to_string_lossy().into_owned()),
                check_after_load: false,
                ..Default::default()
            }),
        )
        .await
        .context("error adding torrent")?
        .into_handle()
        .context("expected handle")?;
    handle.wait_until_initialized().await?;
    let elapsed = boot2_started.elapsed();
    let st = handle.stats();
    println!(
        "LIVE-RESUME: check_after_load=false elapsed_us={} finished={} live={:?}",
        elapsed.as_micros(),
        st.finished,
        st.live.as_ref().map(|_| true)
    );
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Corrupt a byte and recheck: the corruption must be detected.
    let contents = std::fs::read(&file_path)?;
    let mut corrupted = contents.clone();
    corrupted[contents.len() / 2] ^= 0xff;
    std::fs::write(&file_path, corrupted)?;
    let t0 = Instant::now();
    handle.recheck().context("recheck")?;
    handle.wait_until_initialized().await?;
    let st = handle.stats();
    println!(
        "LIVE-RECHECK-CORRUPT: elapsed_ms={} progress_bytes={} total_bytes={} finished={}",
        t0.elapsed().as_millis(),
        st.progress_bytes,
        st.total_bytes,
        st.finished
    );

    // Restore and recheck again: back to fully verified and live.
    std::fs::write(&file_path, contents)?;
    let t0 = Instant::now();
    handle.recheck().context("recheck")?;
    handle.wait_until_initialized().await?;
    let st = handle.stats();
    println!(
        "LIVE-RECHECK-RESTORED: elapsed_ms={} progress_bytes={} total_bytes={} finished={}",
        t0.elapsed().as_millis(),
        st.progress_bytes,
        st.total_bytes,
        st.finished
    );
    session.stop().await;
    println!(
        "LIVE: done. Data at {outdir:?}, persistence at {session_dir:?}, RUST_LOG evidence above"
    );
    Ok(())
}
