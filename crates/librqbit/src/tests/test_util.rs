use std::{
    io::Write,
    path::{Path, PathBuf},
};

use rand::RngCore;

pub fn create_new_file_with_random_content(path: &Path, mut size: usize) {
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .unwrap();

    let mut write_buf = [0; 8192];
    while size > 0 {
        rand::thread_rng().fill_bytes(&mut write_buf[..]);
        let written = file.write(&write_buf[..size.min(8192)]).unwrap();
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
        std::fs::remove_dir_all(self.name()).unwrap();
    }
}
