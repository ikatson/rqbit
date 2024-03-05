use std::{io::Write, path::Path};

use librqbit_core::Id20;
use rand::{RngCore, SeedableRng};
use tempfile::TempDir;

pub fn create_new_file_with_random_content(path: &Path, mut size: usize) {
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .unwrap();

    eprintln!("creating temp file {:?}", path);

    const BUF_SIZE: usize = 8192 * 16;
    let mut rng = rand::rngs::SmallRng::from_entropy();
    let mut write_buf = [0; BUF_SIZE];
    while size > 0 {
        rng.fill_bytes(&mut write_buf[..]);
        let written = file.write(&write_buf[..size.min(BUF_SIZE)]).unwrap();
        size -= written;
    }
}

pub fn create_default_random_dir_with_torrents(
    num_files: usize,
    file_size: usize,
    tempdir_prefix: Option<&str>,
) -> TempDir {
    let dir = TempDir::with_prefix(tempdir_prefix.unwrap_or("rqbit_test")).unwrap();
    dbg!(dir.path());
    for f in 0..num_files {
        create_new_file_with_random_content(&dir.path().join(&format!("{f}.data")), file_size);
    }
    dir
}

#[derive(Debug)]
pub struct TestPeerMetadata {
    pub server_id: u8,
    pub max_random_sleep_ms: u8,
}

impl TestPeerMetadata {
    pub fn as_peer_id(&self) -> Id20 {
        let mut peer_id = Id20::default();
        peer_id.0[0] = self.server_id;
        peer_id.0[1] = self.max_random_sleep_ms;
        peer_id
    }

    pub fn from_peer_id(peer_id: Id20) -> Self {
        Self {
            server_id: peer_id.0[0],
            max_random_sleep_ms: peer_id.0[1],
        }
    }

    pub fn disconnect_probability(&self) -> f64 {
        if self.server_id % 2 == 0 {
            return 0.05f64;
        }
        0f64
    }
}
