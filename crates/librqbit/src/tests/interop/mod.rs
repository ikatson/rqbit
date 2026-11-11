//! Interoperability tests against libtorrent (the engine behind qBittorrent and Deluge), driven
//! through its Python bindings.
//!
//! They are ignored by default, because they need a Python interpreter that has the
//! `libtorrent` module:
//!
//! ```text
//! python3 -m venv /tmp/lt && /tmp/lt/bin/pip install libtorrent
//! RQBIT_TEST_LIBTORRENT_PYTHON=/tmp/lt/bin/python cargo test -p librqbit interop -- --ignored
//! ```
//!
//! A test runs a Python script as a child process and talks JSON lines with it: the script
//! reports what happened on stdout, one object with an `"event"` key per line, and takes
//! commands the same way on stdin. `common.py` is prepended to every script.

// Helpers shared by the interop test modules; not every module uses all of them.
#![allow(dead_code)]

use std::{path::PathBuf, process::Stdio, time::Duration};

use anyhow::{Context, bail};
use serde_json::Value;
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::timeout,
};

mod fast_extension;
mod smoke;
mod super_seeding;

/// The environment variable that points at a Python interpreter with `libtorrent` installed.
pub(super) const PYTHON_ENV: &str = "RQBIT_TEST_LIBTORRENT_PYTHON";

const COMMON_PY: &str = include_str!("common.py");

/// A running Python script that drives libtorrent.
pub(super) struct Libtorrent {
    // Killed on drop.
    _child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    dir: TempDir,
}

impl Libtorrent {
    /// Starts `script` (Python source, `common.py` is prepended to it) with `args`.
    ///
    /// The script runs in a fresh temporary directory, which is removed when this is dropped.
    pub(super) async fn spawn(script: &str, args: &[&str]) -> anyhow::Result<Self> {
        let python = std::env::var_os(PYTHON_ENV).with_context(|| {
            format!("{PYTHON_ENV} must point at a Python interpreter that has libtorrent")
        })?;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("script.py");
        tokio::fs::write(&path, format!("{COMMON_PY}\n{script}")).await?;
        let mut child = Command::new(&python)
            // Unbuffered, so that an event is seen as soon as it is printed.
            .arg("-u")
            .arg(&path)
            .args(args)
            .current_dir(dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("error running {python:?}"))?;
        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("no stdout")?).lines();
        Ok(Self {
            _child: child,
            stdin,
            stdout,
            dir,
        })
    }

    /// The directory the script runs in.
    pub(super) fn dir(&self) -> PathBuf {
        self.dir.path().to_owned()
    }

    /// The next event the script reports. Lines that are not JSON objects are skipped.
    pub(super) async fn recv(&mut self, within: Duration) -> anyhow::Result<Value> {
        let next = async {
            loop {
                let line = self
                    .stdout
                    .next_line()
                    .await?
                    .context("the script exited")?;
                match serde_json::from_str::<Value>(&line) {
                    Ok(value) if value.is_object() => return anyhow::Ok(value),
                    _ => tracing::debug!(line, "libtorrent script"),
                }
            }
        };
        timeout(within, next)
            .await
            .context("timed out waiting for the script")?
    }

    /// Waits for the event called `event`, skipping the others. Fails on an `"error"` event.
    pub(super) async fn expect(&mut self, event: &str, within: Duration) -> anyhow::Result<Value> {
        let wait = async {
            loop {
                // The outer timeout bounds the whole wait.
                let value = self.recv(Duration::from_secs(3600)).await?;
                match value.get("event").and_then(Value::as_str) {
                    Some(name) if name == event => return anyhow::Ok(value),
                    Some("error") => bail!("the script failed: {value}"),
                    _ => tracing::debug!(%value, "skipped event"),
                }
            }
        };
        timeout(within, wait)
            .await
            .with_context(|| format!("timed out waiting for event {event:?}"))?
    }

    /// Sends a command to the script.
    pub(super) async fn send(&mut self, command: &Value) -> anyhow::Result<()> {
        let mut line = serde_json::to_vec(command)?;
        line.push(b'\n');
        self.stdin.write_all(&line).await?;
        self.stdin.flush().await?;
        Ok(())
    }
}

/// A field of an event as u16, such as the port the script listens on.
pub(super) fn port_of(event: &Value, field: &str) -> anyhow::Result<u16> {
    let port = event
        .get(field)
        .and_then(Value::as_u64)
        .with_context(|| format!("no {field:?} in {event}"))?;
    Ok(u16::try_from(port)?)
}
