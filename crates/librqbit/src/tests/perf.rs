use std::path::PathBuf;

use librqbit_core::torrent_metainfo::TorrentVersion;
use tracing::info;

use crate::{
    CreateTorrentOptions, create_torrent, spawn_utils::BlockingSpawner,
    tests::test_util::setup_test_logging,
};

fn large_file_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("RQBIT_LARGE_FILE_PATH") {
        return Some(PathBuf::from(path));
    }
    let default = PathBuf::from("tests/large_files/large.bin");
    default.exists().then_some(default)
}

#[tokio::test]
async fn test_perf_large_file_v2_optional() {
    setup_test_logging();

    let Some(path) = large_file_path() else {
        eprintln!("skipping: set RQBIT_LARGE_FILE_PATH or create tests/large_files/large.bin");
        return;
    };
    if !path.exists() {
        eprintln!("skipping: large file not found at {}", path.display());
        return;
    }

    let start = std::time::Instant::now();
    let torrent = create_torrent(
        &path,
        CreateTorrentOptions {
            version: Some(TorrentVersion::V2Only),
            piece_length: Some(1024 * 1024),
            ..Default::default()
        },
        &BlockingSpawner::new(1),
    )
    .await
    .unwrap();
    let elapsed = start.elapsed();
    info!(?path, ?elapsed, "created v2-only torrent for large file");

    let bytes = torrent.as_bytes().unwrap();
    let parsed = librqbit_core::torrent_metainfo::torrent_from_bytes(&bytes).unwrap();
    assert_eq!(parsed.version(), Some(TorrentVersion::V2Only));
    parsed
        .validate_v2_piece_layers()
        .expect("piece_layers validation should pass");
}
