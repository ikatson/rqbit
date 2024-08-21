use std::{io::Write, path::Path};

use anyhow::Context;
use axum::{response::IntoResponse, routing::get, Router};
use librqbit_core::Id20;
use rand::{thread_rng, Rng, RngCore, SeedableRng};
use tempfile::TempDir;
use tracing::{debug, info};

pub fn create_new_file_with_random_content(path: &Path, mut size: usize) {
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .unwrap();

    debug!(?path, "creating temp file");

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
    info!(path=?dir.path(), "created tempdir");
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
    pub fn good() -> Self {
        Self {
            server_id: 0,
            max_random_sleep_ms: 0,
        }
    }

    pub fn as_peer_id(&self) -> Id20 {
        let mut peer_id = Id20::default();
        thread_rng().fill(&mut peer_id.0);
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
        if self.server_id % 2 == 1 {
            return 0.05f64;
        }
        0f64
    }

    pub fn bad_data_probability(&self) -> f64 {
        if self.server_id % 2 == 1 {
            return 0.05f64;
        }
        0f64
    }
}

async fn debug_server() -> anyhow::Result<()> {
    async fn backtraces() -> impl IntoResponse {
        #[cfg(feature = "async-bt")]
        {
            async_backtrace::taskdump_tree(true)
        }
        #[cfg(not(feature = "async-bt"))]
        {
            use crate::ApiError;
            ApiError::from(anyhow::anyhow!(
                "backtraces not enabled, enable async-bt feature"
            ))
        }
    }

    let app = Router::new().route("/backtrace", get(backtraces));
    let app = app.into_make_service();

    let addr = "127.0.0.1:3032";

    info!(%addr, "starting HTTP server");

    use tokio::net::TcpListener;

    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("error binding to {addr}"))?;
    axum::serve(listener, app).await?;
    Ok(())
}

pub fn spawn_debug_server() {
    tokio::spawn(debug_server());
}
