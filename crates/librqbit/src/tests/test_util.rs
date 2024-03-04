use std::{
    io::Write,
    path::{Path, PathBuf},
};

use librqbit_core::Id20;
use rand::RngCore;
use tracing::info;

pub fn create_new_file_with_random_content(path: &Path, mut size: usize) {
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .unwrap();

    eprintln!("creating temp file {:?}", path);

    const BUF_SIZE: usize = 8192 * 16;
    let mut rng = rand::rngs::OsRng;
    let mut write_buf = [0; BUF_SIZE];
    while size > 0 {
        rng.fill_bytes(&mut write_buf[..]);
        let written = file.write(&write_buf[..size.min(BUF_SIZE)]).unwrap();
        size -= written;
    }
}

pub fn create_default_random_dir_with_torrents(num_files: usize, file_size: usize) -> NamedTempDir {
    let dir = NamedTempDir::new().unwrap();
    dbg!(dir.name());
    for f in 0..num_files {
        create_new_file_with_random_content(&dir.name().join(&format!("{f}.data")), file_size);
    }
    dir
}

// TODO: once online, remove this in favor of crate
pub struct NamedTempDir {
    name: PathBuf,
}

impl NamedTempDir {
    pub fn new() -> anyhow::Result<Self> {
        let out = std::process::Command::new("mktemp")
            .arg("-d")
            .arg("rqbit_test_XXXXXX")
            .arg("--tmpdir")
            .output()
            .unwrap();
        let path = out.stdout;
        assert!(!path.is_empty());
        let path = String::from_utf8(path).unwrap().trim_end().to_owned();
        let path = PathBuf::from(path);
        Ok(Self { name: path })
    }

    pub fn name(&self) -> &Path {
        &self.name
    }
}

impl Drop for NamedTempDir {
    fn drop(&mut self) {
        info!(name = ?self.name(), "removing NamedTempDir");
        std::fs::remove_dir_all(self.name()).unwrap();
        info!(name = ?self.name(), "removed NamedTempDir");
    }
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
            return 0.005f64;
        }
        0f64
    }
}
