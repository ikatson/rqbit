//! The HTTP endpoints of the storage operations.
//!
//! The API is served on a loopback port picked by the OS and driven with reqwest, exactly as a
//! remote client would, so routing, JSON parsing and error statuses are all covered.
#![cfg(feature = "http-api")]

use librqbit_dualstack_sockets::{BindOpts, TcpListener};
use reqwest::StatusCode;

use super::*;
use crate::{
    Api,
    http_api::{HttpApi, HttpApiOptions},
};

struct Served {
    base: String,
    client: reqwest::Client,
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Drop for Served {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Served {
    async fn post(
        &self,
        handle: &ManagedTorrent,
        action: &str,
        body: Option<serde_json::Value>,
    ) -> anyhow::Result<(StatusCode, String)> {
        let url = format!("{}/torrents/{}/{action}", self.base, handle.id());
        let mut req = self.client.post(url);
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req.send().await?;
        let status = resp.status();
        Ok((status, resp.text().await?))
    }
}

async fn serve(session: &Arc<Session>, read_only: bool) -> anyhow::Result<Served> {
    let listener = TcpListener::bind_tcp(
        (Ipv4Addr::LOCALHOST, 0).into(),
        BindOpts {
            request_dualstack: false,
            reuseport: false,
            device: None,
        },
    )
    .context("binding the HTTP API")?;
    let addr = listener.bind_addr();
    let api = Api::new(
        session.clone(),
        None,
        #[cfg(feature = "tracing-subscriber-utils")]
        None,
    );
    let http = HttpApi::new(
        api,
        Some(HttpApiOptions {
            read_only,
            ..Default::default()
        }),
    );
    let server = tokio::spawn(http.make_http_api_and_run(listener, None));
    Ok(Served {
        base: format!("http://{addr}"),
        client: reqwest::Client::new(),
        server,
    })
}

// A paused, complete torrent of NESTED_FILES stored in `<dir>/old`.
async fn paused_torrent(
    dir: &Path,
) -> anyhow::Result<(Content, Arc<Session>, ManagedTorrentHandle, PathBuf)> {
    let content = Content::new(NESTED_FILES, true);
    let old = dir.join("old");
    content.write_to(&old);
    let session = new_session(dir, Default::default()).await?;
    let handle = add(
        &session,
        &content,
        &old,
        AddTorrentOptions {
            paused: true,
            ..Default::default()
        },
    )
    .await?;
    assert_eq!(have_pieces(&handle)?, content.all_pieces());
    Ok((content, session, handle, old))
}

// A read-only API has no storage operation routes: every call is refused and nothing on disk
// or in the torrent changes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_http_read_only_rejects_storage_ops() -> anyhow::Result<()> {
    run(async {
        let dir = TempDir::with_prefix("rqbit_http_read_only")?;
        let (content, session, handle, old) = paused_torrent(dir.path()).await?;
        let new = dir.path().join("new");
        let served = serve(&session, true).await?;

        let calls = [
            ("force_recheck", None),
            (
                "move_storage",
                Some(serde_json::json!({ "output_folder": new, "mode": "overwrite" })),
            ),
            (
                "rename_file",
                Some(serde_json::json!({ "file_index": 0, "path": "renamed.bin" })),
            ),
        ];
        for (action, body) in calls {
            let (status, text) = served.post(&handle, action, body).await?;
            assert!(
                status == StatusCode::NOT_FOUND || status == StatusCode::METHOD_NOT_ALLOWED,
                "{action}: {status} {text}"
            );
        }

        assert_eq!(handle.output_folder(), old);
        content.assert_at(&old, &[])?;
        assert!(!new.exists());
        assert!(!old.join("renamed.bin").exists());
        assert!(!check_running(&handle));
        assert!(is_paused(&handle));
        assert_eq!(have_pieces(&handle)?, content.all_pieces());
        Ok(())
    })
    .await
}

// Without `mode` a move is `fail_if_exists`: refused while a file of the torrent is at the
// destination, carried out once it is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_http_move_storage_default_mode() -> anyhow::Result<()> {
    run(async {
        let dir = TempDir::with_prefix("rqbit_http_move_default")?;
        let (content, session, handle, old) = paused_torrent(dir.path()).await?;
        let new = dir.path().join("new");
        std::fs::create_dir_all(&new)?;
        std::fs::write(new.join("a.bin"), b"someone else's file")?;
        let served = serve(&session, false).await?;
        let body = serde_json::json!({ "output_folder": new });

        let (status, text) = served
            .post(&handle, "move_storage", Some(body.clone()))
            .await?;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
        assert_eq!(handle.output_folder(), old);
        content.assert_at(&old, &[])?;
        assert_eq!(std::fs::read(new.join("a.bin"))?, b"someone else's file");
        assert!(!new.join("sub").exists());

        std::fs::remove_file(new.join("a.bin"))?;
        let (status, text) = served.post(&handle, "move_storage", Some(body)).await?;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(handle.output_folder(), new);
        content.assert_at(&new, &[])?;
        no_torrent_files_at(&content, &old)?;
        assert!(is_paused(&handle));
        Ok(())
    })
    .await
}

// An explicit mode is honoured, and an unknown one is refused before anything is touched,
// with a message naming the field and the accepted values.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_http_move_storage_modes() -> anyhow::Result<()> {
    run(async {
        let dir = TempDir::with_prefix("rqbit_http_move_modes")?;
        let (content, session, handle, old) = paused_torrent(dir.path()).await?;
        let new = dir.path().join("new");
        std::fs::create_dir_all(&new)?;
        std::fs::write(new.join("a.bin"), b"stale")?;
        let served = serve(&session, false).await?;

        let (status, text) = served
            .post(
                &handle,
                "move_storage",
                Some(serde_json::json!({ "output_folder": new, "mode": "clobber" })),
            )
            .await?;
        assert!(status.is_client_error(), "{status} {text}");
        assert!(text.contains("mode"), "{text}");
        assert!(text.contains("clobber"), "{text}");
        assert!(text.contains("fail_if_exists"), "{text}");
        assert_eq!(handle.output_folder(), old);
        content.assert_at(&old, &[])?;
        assert_eq!(std::fs::read(new.join("a.bin"))?, b"stale");

        let (status, text) = served
            .post(
                &handle,
                "move_storage",
                Some(serde_json::json!({ "output_folder": new, "mode": "overwrite" })),
            )
            .await?;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(handle.output_folder(), new);
        content.assert_at(&new, &[])?;
        no_torrent_files_at(&content, &old)?;
        Ok(())
    })
    .await
}

// A rename moves the file on disk; a file index past the end is a client error that changes
// nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_http_rename_file() -> anyhow::Result<()> {
    run(async {
        let dir = TempDir::with_prefix("rqbit_http_rename")?;
        let (content, session, handle, old) = paused_torrent(dir.path()).await?;
        let served = serve(&session, false).await?;
        let num_files = handle.metadata.load().as_ref().unwrap().file_infos.len();

        let (status, text) = served
            .post(
                &handle,
                "rename_file",
                Some(serde_json::json!({ "file_index": num_files, "path": "x.bin" })),
            )
            .await?;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
        assert!(!old.join("x.bin").exists());
        content.assert_at(&old, &[])?;

        let c = content.file("sub/c.bin");
        let (status, text) = served
            .post(
                &handle,
                "rename_file",
                Some(serde_json::json!({ "file_index": c.index, "path": "moved/c2.bin" })),
            )
            .await?;
        assert_eq!(status, StatusCode::OK, "{text}");
        content.assert_at(&old, &[(c.index, "moved/c2.bin")])?;
        assert!(!old.join("sub/c.bin").exists());
        Ok(())
    })
    .await
}

// A recheck over HTTP finds a piece damaged on disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_http_force_recheck() -> anyhow::Result<()> {
    run(async {
        let dir = TempDir::with_prefix("rqbit_http_recheck")?;
        let (content, session, handle, old) = paused_torrent(dir.path()).await?;
        let served = serve(&session, false).await?;
        content.corrupt(&old, 1);

        let (status, text) = served.post(&handle, "force_recheck", None).await?;
        assert_eq!(status, StatusCode::OK, "{text}");
        wait_initialized(&handle).await?;
        assert_eq!(have_pieces(&handle)?, without(content.all_pieces(), &[1]));
        assert!(is_paused(&handle));
        Ok(())
    })
    .await
}
