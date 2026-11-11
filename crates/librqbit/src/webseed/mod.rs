//! Web seeds (BEP 19, "GetRight style"): HTTP(S) servers that publish the content of a torrent
//! as plain files. A torrent downloads from them alongside its peers.
//!
//! [`WebSeeds`] is the list of a torrent's web seeds and what is known about each of them. It
//! lives as long as the torrent, across pause / resume.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use crate::{ManagedTorrent, Session, torrent_state::ManagedTorrentHandle};

use librqbit_core::web_seed::{MAX_WEB_SEEDS, is_web_seed_url, push_web_seed};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// What a web seed of a torrent is doing.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebSeedState {
    /// Not being downloaded from: the torrent is paused or finished, or other web seeds are in
    /// use.
    Idle,
    /// Asking the web seed for pieces.
    Downloading,
    /// The web seed failed in a way that may pass (timeout, connection error, 5xx, 429): it is
    /// asked again after a delay that grows with each failure.
    BackingOff,
    /// The web seed is not used until the torrent is started again: its answers were unusable
    /// (no range support, wrong length, data that failed the hash check), or web seeds are
    /// disabled for the torrent or the session.
    Disabled,
}

/// The state of one web seed of a torrent, see [`crate::ManagedTorrent::web_seeds`].
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct WebSeedStats {
    pub url: String,
    pub state: WebSeedState,
    /// Bytes received from the web seed that were written to the torrent's files, since the
    /// torrent was added in this process.
    pub downloaded_bytes: u64,
    /// The last error seen from the web seed in the current run, if any.
    pub last_error: Option<String>,
}

#[derive(Debug)]
struct Status {
    state: WebSeedState,
    last_error: Option<String>,
}

/// One web seed of a torrent.
#[derive(Debug)]
pub(crate) struct WebSeed {
    url: String,
    status: Mutex<Status>,
    downloaded_bytes: AtomicU64,
    /// Cancelled when the web seed is removed from the torrent: its worker stops at once.
    removed: CancellationToken,
}

impl WebSeed {
    fn new(url: String) -> Self {
        Self {
            url,
            status: Mutex::new(Status {
                state: WebSeedState::Idle,
                last_error: None,
            }),
            downloaded_bytes: AtomicU64::new(0),
            removed: CancellationToken::new(),
        }
    }

    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    pub(crate) fn state(&self) -> WebSeedState {
        self.status.lock().state
    }

    pub(crate) fn set_state(&self, state: WebSeedState) {
        self.status.lock().state = state;
    }

    /// Sets the state and remembers the error that led to it.
    pub(crate) fn set_error(&self, state: WebSeedState, error: String) {
        let mut g = self.status.lock();
        g.state = state;
        g.last_error = Some(error);
    }

    /// Forgets what happened in the previous run: a new run tries every web seed again.
    pub(crate) fn reset(&self) {
        let mut g = self.status.lock();
        g.state = WebSeedState::Idle;
        g.last_error = None;
    }

    pub(crate) fn add_downloaded(&self, bytes: u64) {
        self.downloaded_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn removed(&self) -> &CancellationToken {
        &self.removed
    }

    fn stats(&self, disabled: bool) -> WebSeedStats {
        let g = self.status.lock();
        WebSeedStats {
            url: self.url.clone(),
            state: if disabled {
                WebSeedState::Disabled
            } else {
                g.state
            },
            downloaded_bytes: self.downloaded_bytes.load(Ordering::Relaxed),
            last_error: g.last_error.clone(),
        }
    }
}

/// The web seeds of a torrent.
pub(crate) struct WebSeeds {
    /// Web seeds are disabled for this torrent or its session: they are listed (and persisted)
    /// but never contacted.
    disabled: bool,
    list: RwLock<Vec<Arc<WebSeed>>>,
    /// Woken when a web seed is added or removed.
    changed: Notify,
}

impl WebSeeds {
    /// `urls` must be usable web seed URLs (see [`push_web_seed`]).
    pub(crate) fn new(urls: Vec<String>, disabled: bool) -> Self {
        Self {
            disabled,
            list: RwLock::new(
                urls.into_iter()
                    .map(|url| Arc::new(WebSeed::new(url)))
                    .collect(),
            ),
            changed: Notify::new(),
        }
    }

    pub(crate) fn is_disabled(&self) -> bool {
        self.disabled
    }

    pub(crate) fn urls(&self) -> Vec<String> {
        self.list.read().iter().map(|w| w.url.clone()).collect()
    }

    pub(crate) fn stats(&self) -> Vec<WebSeedStats> {
        self.list
            .read()
            .iter()
            .map(|w| w.stats(self.disabled))
            .collect()
    }

    /// The web seeds, in the order they were added.
    pub(crate) fn snapshot(&self) -> Vec<Arc<WebSeed>> {
        self.list.read().clone()
    }

    pub(crate) fn changed(&self) -> &Notify {
        &self.changed
    }

    /// Adds a web seed. Returns false if it is there already.
    pub(crate) fn add(&self, url: &str) -> anyhow::Result<bool> {
        let url = url.trim();
        if !is_web_seed_url(url) {
            anyhow::bail!("not a usable web seed URL (absolute http or https): {url:?}");
        }
        {
            let mut list = self.list.write();
            if list.iter().any(|w| w.url == url) {
                return Ok(false);
            }
            if list.len() >= MAX_WEB_SEEDS {
                anyhow::bail!("a torrent can have at most {MAX_WEB_SEEDS} web seeds");
            }
            list.push(Arc::new(WebSeed::new(url.to_owned())));
        }
        self.changed.notify_waiters();
        Ok(true)
    }

    /// Removes a web seed and stops downloading from it. Returns false if it was not there.
    pub(crate) fn remove(&self, url: &str) -> bool {
        let url = url.trim();
        let removed = {
            let mut list = self.list.write();
            let Some(pos) = list.iter().position(|w| w.url == url) else {
                return false;
            };
            list.remove(pos)
        };
        removed.removed.cancel();
        self.changed.notify_waiters();
        true
    }
}

/// The web seeds of a new torrent: `own` (the torrent's `url-list` or the magnet's `ws`
/// parameters) then `extra` (given by the host), skipping unusable and duplicate URLs, at most
/// [`MAX_WEB_SEEDS`]. When `replace` is set and there is an `extra` (a torrent added again from
/// a list that already is the merged one, minus what was removed), only `extra` is used.
pub(crate) fn merge_web_seed_urls(
    own: Vec<String>,
    extra: Option<&[String]>,
    replace: bool,
) -> Vec<String> {
    let mut urls = Vec::new();
    let own = match (replace, extra) {
        (true, Some(_)) => Vec::new(),
        _ => own,
    };
    for url in own.iter().chain(extra.unwrap_or_default()) {
        push_web_seed(&mut urls, url);
    }
    urls
}

impl ManagedTorrent {
    /// The web seeds (BEP 19) of the torrent and what each of them is doing.
    pub fn web_seeds(&self) -> Vec<WebSeedStats> {
        self.shared.web_seeds.stats()
    }

    /// The URLs of the torrent's web seeds, as persisted.
    pub(crate) fn web_seed_urls(&self) -> Vec<String> {
        self.shared.web_seeds.urls()
    }
}

impl Session {
    /// Adds a web seed (BEP 19, an `http` or `https` URL) to a torrent. A live torrent starts
    /// using it at once. Adding a web seed the torrent has already does nothing.
    pub async fn add_web_seed(
        &self,
        handle: &ManagedTorrentHandle,
        url: &str,
    ) -> anyhow::Result<()> {
        if handle.shared.web_seeds.add(url)? {
            self.try_update_persistence_metadata(handle).await;
        }
        Ok(())
    }

    /// Removes a web seed from a torrent. Requests in flight to it are cancelled and the
    /// pieces it was downloading go back to the queue.
    pub async fn remove_web_seed(
        &self,
        handle: &ManagedTorrentHandle,
        url: &str,
    ) -> anyhow::Result<()> {
        if !handle.shared.web_seeds.remove(url) {
            anyhow::bail!("the torrent has no web seed {url:?}");
        }
        self.try_update_persistence_metadata(handle).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_merge_web_seed_urls() {
        let own = s(&["http://a/x", "ftp://bad/x"]);
        let extra = s(&["http://b/x", "http://a/x", "nonsense"]);
        assert_eq!(
            merge_web_seed_urls(own.clone(), Some(&extra), false),
            s(&["http://a/x", "http://b/x"])
        );
        assert_eq!(
            merge_web_seed_urls(own.clone(), None, false),
            s(&["http://a/x"])
        );
        // Restoring: the stored list wins, even when empty (everything was removed).
        assert_eq!(
            merge_web_seed_urls(own.clone(), Some(&extra), true),
            s(&["http://b/x", "http://a/x"])
        );
        assert_eq!(merge_web_seed_urls(own.clone(), Some(&[]), true), s(&[]));
        // Restoring state written before web seeds existed: the torrent's own.
        assert_eq!(merge_web_seed_urls(own, None, true), s(&["http://a/x"]));
    }

    #[test]
    fn test_web_seeds_add_remove() {
        let ws = WebSeeds::new(s(&["http://a/x"]), false);
        assert!(!ws.add("http://a/x").unwrap());
        assert!(ws.add(" http://b/x ").unwrap());
        assert!(ws.add("udp://c/x").is_err());
        assert_eq!(ws.urls(), s(&["http://a/x", "http://b/x"]));
        let a = ws.snapshot().remove(0);
        assert!(ws.remove("http://a/x"));
        assert!(a.removed().is_cancelled());
        assert!(!ws.remove("http://a/x"));
        assert_eq!(ws.urls(), s(&["http://b/x"]));
        for i in 0..MAX_WEB_SEEDS - 1 {
            ws.add(&format!("http://h{i}/")).unwrap();
        }
        assert!(ws.add("http://one-too-many/").is_err());
    }

    #[test]
    fn test_disabled_web_seeds_report_disabled() {
        let ws = WebSeeds::new(s(&["http://a/x"]), true);
        assert_eq!(ws.stats()[0].state, WebSeedState::Disabled);
        let ws = WebSeeds::new(s(&["http://a/x"]), false);
        assert_eq!(ws.stats()[0].state, WebSeedState::Idle);
    }
}
