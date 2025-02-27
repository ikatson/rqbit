use std::{
    io::Write,
    path::Path,
    sync::{Arc, Weak},
    time::Duration,
};

use anyhow::bail;
use librqbit_core::{peer_id::generate_peer_id, Id20};
use parking_lot::RwLock;
use rand::{thread_rng, Rng, RngCore, SeedableRng};
use tempfile::TempDir;
use tracing::{info, trace};

pub fn setup_test_logging() {
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "debug,librqbit_core=trace");
    }
    let _ = tracing_subscriber::fmt::try_init();
}

pub fn create_new_file_with_random_content(path: &Path, mut size: usize) {
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .unwrap();

    trace!(?path, "creating temp file");

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
        create_new_file_with_random_content(&dir.path().join(format!("{f}.data")), file_size);
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
        let mut peer_id = generate_peer_id();
        peer_id.0[15..19].copy_from_slice(b"test");
        thread_rng().fill(&mut peer_id.0);
        peer_id.0[14] = self.server_id;
        peer_id.0[13] = self.max_random_sleep_ms;
        peer_id
    }

    pub fn from_peer_id(peer_id: Id20) -> Self {
        if &peer_id.0[15..19] != b"test" {
            return Self::good();
        }
        Self {
            server_id: peer_id.0[14],
            max_random_sleep_ms: peer_id.0[13],
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

#[cfg(feature = "http-api")]
async fn debug_server() -> anyhow::Result<()> {
    use anyhow::Context;
    use axum::{response::IntoResponse, routing::get, Router};
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

#[cfg(not(feature = "http-api"))]
async fn debug_server() -> anyhow::Result<()> {
    Ok(())
}

#[allow(dead_code)]
pub fn spawn_debug_server() -> tokio::task::JoinHandle<anyhow::Result<()>> {
    tokio::spawn(debug_server())
}

pub trait DropPlaceholder: Send + Sync {}
impl<T: Send + Sync> DropPlaceholder for T {}

struct DropCheck {
    obj: Weak<dyn DropPlaceholder>,
    name: String,
}

#[derive(Default, Clone)]
pub struct DropChecks(Arc<RwLock<Vec<DropCheck>>>);

impl DropChecks {
    pub fn add<T: DropPlaceholder + 'static, S: Into<String>>(&self, obj: &Arc<T>, name: S) {
        let weak = Arc::downgrade(obj);
        self.0.write().push(DropCheck {
            obj: weak as Weak<dyn DropPlaceholder>,
            name: name.into(),
        })
    }

    pub fn check(&self) -> anyhow::Result<()> {
        let mut still_running = Vec::new();
        for dc in self.0.read().iter() {
            if dc.obj.upgrade().is_some() {
                still_running.push(dc.name.clone())
            }
        }
        if !still_running.is_empty() {
            anyhow::bail!(
                "still existing objects that were supposed to be dropped: {still_running:#?}"
            )
        }
        Ok(())
    }
}

pub async fn wait_until(
    mut cond: impl FnMut() -> anyhow::Result<()>,
    timeout: Duration,
) -> anyhow::Result<()> {
    let mut interval = tokio::time::interval(Duration::from_millis(10));
    let mut last_err: Option<anyhow::Error> = None;
    let res = tokio::time::timeout(timeout, async {
        loop {
            interval.tick().await;
            match cond() {
                Ok(()) => return Ok::<_, anyhow::Error>(()),
                Err(e) => last_err = Some(e),
            }
        }
    })
    .await;
    if res.is_err() {
        bail!("wait_until timeout: last result = {last_err:?}")
    }
    Ok(())
}

pub async fn wait_until_i_am_the_last_task() -> anyhow::Result<()> {
    let metrics = tokio::runtime::Handle::current().metrics();
    wait_until(
        || {
            let num_alive = metrics.num_alive_tasks();
            if num_alive != 0 {
                bail!("metrics.num_alive_tasks() = {num_alive}, expected 0")
            }
            Ok(())
        },
        // This needs to be higher than the timeout the tasks print "still running"
        Duration::from_secs(15),
    )
    .await
}
