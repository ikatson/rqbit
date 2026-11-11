//! Downloading from web seeds.
//!
//! While a torrent is live, one manager task starts a worker for each web seed in use (at most
//! [`MAX_ACTIVE_WEB_SEEDS`] at a time). A worker reserves a run of pieces through the piece
//! tracker, like a peer does, fetches their bytes with HTTP range requests, one per file the
//! run touches, and feeds them, one chunk at a time, through the write and verify path the
//! peers' data takes. Dropping a worker (the torrent pauses or is deleted, the web seed is
//! removed) drops its HTTP response, which closes the connection.

use std::{sync::Arc, time::Duration};

use anyhow::Context;
use bitvec::{order::Msb0, vec::BitVec};
use buffers::{ByteBuf, ByteBufOwned};
use futures::{StreamExt, stream::FuturesUnordered};
use librqbit_core::{
    constants::CHUNK_SIZE,
    lengths::{ChunkInfo, Lengths, ValidPieceIndex},
    torrent_metainfo::ValidatedTorrentMetaV1Info,
    web_seed::web_seed_file_url,
};
use peer_binary_protocol::Piece;
use reqwest::{
    StatusCode,
    header::{CONTENT_LENGTH, CONTENT_RANGE, HeaderMap, RANGE, RETRY_AFTER},
};
use tracing::{debug, debug_span, trace, warn};

use crate::{
    file_info::FileInfo,
    torrent_state::{
        TorrentStateLive, live::peer::stats::atomic::PeerCountersAtomic as AtomicPeerCounters,
    },
    type_aliases::BF,
};

use super::{PieceHashFailed, WebSeed, WebSeedState};

/// How many web seeds of a torrent are downloaded from at the same time.
pub(crate) const MAX_ACTIVE_WEB_SEEDS: usize = 4;
/// How much a web seed reserves at once: at most this many pieces and this many bytes, but
/// at least one piece. Bounded so that the peers are not starved and a slow web seed does not
/// hold much.
const MAX_RUN_PIECES: u64 = 8;
const MAX_RUN_BYTES: u64 = 2 * 1024 * 1024;
/// After this many pieces that failed the hash check, a web seed is disabled for the run.
pub(crate) const MAX_HASH_FAILURES: u32 = 2;
/// How long a worker that found nothing to do waits before looking again, unless woken.
const IDLE_RECHECK: Duration = Duration::from_secs(5);
/// Waiting for a response, or for the next bytes of a body, when the torrent does not set
/// `peer_read_write_timeout`.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const BACKOFF_FIRST: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);
/// The longest `Retry-After` honoured.
const RETRY_AFTER_MAX: Duration = Duration::from_secs(10 * 60);

static ZEROS: [u8; CHUNK_SIZE as usize] = [0; CHUNK_SIZE as usize];

/// Starts downloading from the web seeds of a torrent that just went live. Stops when the live
/// state is cancelled (pause, delete).
pub(crate) fn spawn_web_seeds(state: &Arc<TorrentStateLive>) {
    let seeds = &state.shared.web_seeds;
    if seeds.is_disabled() {
        return;
    }
    // A new run tries every web seed again.
    for seed in seeds.snapshot() {
        seed.reset();
    }
    state.spawn(
        debug_span!(parent: state.shared.span.clone(), "web_seeds"),
        format!("[{}]web_seeds", state.shared.id),
        manage(state.clone()),
    );
}

/// Keeps a worker running for each usable web seed, up to [`MAX_ACTIVE_WEB_SEEDS`].
async fn manage(state: Arc<TorrentStateLive>) -> crate::Result<()> {
    let seeds = &state.shared.web_seeds;
    let mut running: Vec<Arc<WebSeed>> = Vec::new();
    let mut workers = FuturesUnordered::new();
    loop {
        let changed = seeds.changed().notified();
        tokio::pin!(changed);
        changed.as_mut().enable();

        for seed in seeds.snapshot() {
            if running.len() >= MAX_ACTIVE_WEB_SEEDS {
                break;
            }
            if seed.state() == WebSeedState::Disabled
                || seed.removed().is_cancelled()
                || running.iter().any(|r| Arc::ptr_eq(r, &seed))
            {
                continue;
            }
            running.push(seed.clone());
            let state = state.clone();
            workers.push(async move {
                run_worker(&state, &seed).await;
                seed
            });
        }

        tokio::select! {
            _ = &mut changed => {}
            Some(done) = workers.next() => running.retain(|r| !Arc::ptr_eq(r, &done)),
        }
    }
}

/// Puts things in order however a worker ends, also when its future is dropped.
struct WorkerGuard<'a> {
    state: &'a TorrentStateLive,
    seed: &'a WebSeed,
}

impl Drop for WorkerGuard<'_> {
    fn drop(&mut self) {
        // After a pause the piece tracker is gone, and with it what the web seed held.
        if let Err(e) = self.state.web_seed_release(self.seed.handle()) {
            debug!(
                url = self.seed.url(),
                "not giving web seed pieces back: {e:#}"
            );
        }
        if self.seed.state() != WebSeedState::Disabled {
            self.seed.set_state(WebSeedState::Idle);
        }
    }
}

async fn run_worker(state: &TorrentStateLive, seed: &WebSeed) {
    let _guard = WorkerGuard { state, seed };
    let Some(mut worker) = Worker::new(state, seed) else {
        return;
    };
    tokio::select! {
        _ = seed.removed().cancelled() => {}
        r = worker.run() => {
            if let Err(e) = r {
                warn!(url = seed.url(), "web seed worker failed: {e:#}");
                seed.set_error(WebSeedState::Disabled, format!("{e:#}"));
            }
        }
    }
}

/// Why fetching a run of pieces stopped early.
#[derive(Debug)]
enum Failure {
    /// May pass: timeout, connection error, 5xx, 429. `retry_after` is the server's wish.
    Transient {
        error: String,
        retry_after: Option<Duration>,
    },
    /// The web seed does not have this file (404, 403, 410, 416).
    FileUnavailable { file: usize, error: String },
    /// The web seed's answers cannot be used (no range support, wrong length, ...).
    Unusable(String),
    /// The data of this piece failed the hash check.
    HashFailed(ValidPieceIndex),
    /// Someone else has the piece being received now (a peer stole it, or it is not wanted any
    /// more).
    Lost,
    /// Not the web seed's fault (writing to disk, the torrent going away).
    Local(anyhow::Error),
}

impl Failure {
    fn transient(error: impl Into<String>) -> Self {
        Self::Transient {
            error: error.into(),
            retry_after: None,
        }
    }
}

struct Worker<'a> {
    state: &'a TorrentStateLive,
    seed: &'a WebSeed,
    client: reqwest::Client,
    /// The URL of every file of the torrent on this web seed; `None` for padding and empty
    /// files, which are never requested.
    urls: Vec<Option<String>>,
    /// Files this web seed answered "not found" (or alike) for.
    unavailable: Vec<bool>,
    /// Pieces with bytes in an unavailable file: the picker asks about every queued piece
    /// under the torrent's lock, so this is a lookup.
    unservable: BF,
    counters: AtomicPeerCounters,
    timeout: Duration,
    max_pieces: usize,
    hash_failures: u32,
    transient_failures: u32,
    /// Files found missing since the last run that went through.
    file_misses: u32,
}

impl<'a> Worker<'a> {
    fn new(state: &'a TorrentStateLive, seed: &'a WebSeed) -> Option<Self> {
        let client = state.shared.session.upgrade()?.reqwest_client.clone();
        let urls = file_urls(seed.url(), state.info(), state.file_infos());
        let piece_length = u64::from(state.lengths().default_piece_length()).max(1);
        Some(Self {
            state,
            seed,
            client,
            unavailable: vec![false; urls.len()],
            unservable: BitVec::<u8, Msb0>::repeat(
                false,
                usize::try_from(state.lengths().total_pieces()).unwrap_or(0),
            )
            .into_boxed_bitslice(),
            urls,
            counters: AtomicPeerCounters::default(),
            timeout: state
                .shared
                .options
                .peer_read_write_timeout
                .unwrap_or(DEFAULT_TIMEOUT),
            max_pieces: usize::try_from((MAX_RUN_BYTES / piece_length).clamp(1, MAX_RUN_PIECES))
                .unwrap_or(1),
            hash_failures: 0,
            transient_failures: 0,
            file_misses: 0,
        })
    }

    /// Whether every file `piece` has bytes in may be asked from this web seed.
    fn can_serve(&self, piece: ValidPieceIndex) -> bool {
        !self.unservable.get(piece.get_usize()).is_some_and(|b| *b)
    }

    /// Remembers that this web seed lacks `file`. Whether it has any file left.
    fn mark_unavailable(&mut self, file: usize) -> bool {
        if let Some(u) = self.unavailable.get_mut(file) {
            *u = true;
        }
        if let Some(info) = self.state.file_infos().get(file)
            && let Some(bits) = self.unservable.get_mut(info.piece_range_usize())
        {
            bits.fill(true);
        }
        self.urls
            .iter()
            .zip(&self.unavailable)
            .any(|(url, unavailable)| url.is_some() && !unavailable)
    }

    async fn run(&mut self) -> anyhow::Result<()> {
        loop {
            let notified = self.state.new_pieces_notify().notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let run = self.state.web_seed_reserve(
                self.seed.handle(),
                self.counters.average_piece_download_time(),
                |p| self.can_serve(p),
                self.max_pieces,
            )?;
            if run.is_empty() {
                self.seed.set_state(WebSeedState::Idle);
                tokio::select! {
                    _ = &mut notified => {}
                    _ = tokio::time::sleep(IDLE_RECHECK) => {}
                }
                continue;
            }

            self.seed.set_state(WebSeedState::Downloading);
            let result = self.fetch_run(&run).await;
            // Whatever was not completed goes back to the queue.
            self.state.web_seed_release(self.seed.handle())?;

            match result {
                Ok(()) => {
                    self.transient_failures = 0;
                    self.file_misses = 0;
                }
                Err(Failure::Lost) => {}
                Err(Failure::Transient { error, retry_after }) => {
                    self.transient_failures = self.transient_failures.saturating_add(1);
                    let delay = transient_delay(retry_after, self.transient_failures);
                    debug!(url = self.seed.url(), ?delay, "web seed failed: {error}");
                    self.seed.set_error(WebSeedState::BackingOff, error);
                    tokio::time::sleep(delay).await;
                }
                Err(Failure::FileUnavailable { file, error }) => {
                    debug!(
                        url = self.seed.url(),
                        file, "web seed lacks a file: {error}"
                    );
                    if !self.mark_unavailable(file) {
                        self.seed.set_error(
                            WebSeedState::Disabled,
                            format!("{error}; the web seed has none of the files"),
                        );
                        return Ok(());
                    }
                    // Like libtorrent, wait before asking again, so that a wrong base URL
                    // does not turn into a request for every file at once.
                    self.file_misses = self.file_misses.saturating_add(1);
                    let delay = backoff(self.file_misses);
                    self.seed.set_error(WebSeedState::BackingOff, error);
                    tokio::time::sleep(delay).await;
                }
                Err(Failure::Unusable(error)) => {
                    warn!(url = self.seed.url(), "disabling web seed: {error}");
                    self.seed.set_error(WebSeedState::Disabled, error);
                    return Ok(());
                }
                Err(Failure::HashFailed(piece)) => {
                    self.hash_failures += 1;
                    let error = format!("piece {piece} failed the hash check");
                    warn!(url = self.seed.url(), "{error}");
                    if self.hash_failures >= MAX_HASH_FAILURES {
                        self.seed.set_error(
                            WebSeedState::Disabled,
                            format!("{error} ({} pieces failed)", self.hash_failures),
                        );
                        return Ok(());
                    }
                    self.seed.set_error(WebSeedState::Downloading, error);
                }
                Err(Failure::Local(e)) => return Err(e),
            }
        }
    }

    /// Downloads the pieces of `run`, which this web seed holds.
    async fn fetch_run(&self, run: &[ValidPieceIndex]) -> Result<(), Failure> {
        let lengths = self.state.lengths();
        let chunks: Vec<ChunkInfo> = run
            .iter()
            .flat_map(|p| lengths.iter_chunk_infos(*p))
            .collect();
        let mut feeder = Feeder {
            state: self.state,
            seed: self.seed,
            counters: &self.counters,
            chunks: chunks.iter(),
            current: None,
            buf: Vec::with_capacity(CHUNK_SIZE as usize),
            received_in_buf: 0,
        };
        feeder.current = feeder.chunks.next().copied();

        for (start, end) in byte_segments(lengths, &chunks) {
            let mut pos = start;
            for (file, info) in self.state.file_infos().iter().enumerate() {
                let file_end = info.offset_in_torrent + info.len;
                if info.len == 0 || file_end <= pos || info.offset_in_torrent >= end {
                    continue;
                }
                // Bytes between files belong to no file (none should be in a piece).
                if info.offset_in_torrent > pos {
                    feeder.feed_zeros(info.offset_in_torrent - pos).await?;
                    pos = info.offset_in_torrent;
                }
                let part_end = file_end.min(end);
                match self.urls.get(file).and_then(|u| u.as_deref()) {
                    None => feeder.feed_zeros(part_end - pos).await?,
                    Some(url) => {
                        if self.unavailable.get(file).copied().unwrap_or(false) {
                            return Err(Failure::FileUnavailable {
                                file,
                                error: "the web seed does not have the file".to_owned(),
                            });
                        }
                        self.fetch_file_part(
                            file,
                            url,
                            pos - info.offset_in_torrent,
                            part_end - pos,
                            info.len,
                            &mut feeder,
                        )
                        .await?
                    }
                }
                pos = part_end;
                if pos >= end {
                    break;
                }
            }
            if pos < end {
                feeder.feed_zeros(end - pos).await?;
            }
        }
        Ok(())
    }

    /// Fetches `len` bytes of file `file` from `offset` and feeds them to `feeder`.
    async fn fetch_file_part(
        &self,
        file: usize,
        url: &str,
        offset: u64,
        len: u64,
        file_len: u64,
        feeder: &mut Feeder<'_>,
    ) -> Result<(), Failure> {
        trace!(url, offset, len, "web seed request");
        let request = self
            .client
            .get(url)
            .header(RANGE, format!("bytes={}-{}", offset, offset + len - 1));
        let mut response = match tokio::time::timeout(self.timeout, request.send()).await {
            Err(_) => return Err(Failure::transient(format!("{url}: no response in time"))),
            Ok(Err(e)) => return Err(Failure::transient(format!("{url}: {e:#}"))),
            Ok(Ok(r)) => r,
        };
        check_response(
            response.status(),
            response.headers(),
            file,
            offset,
            len,
            file_len,
        )
        .map_err(|f| with_url(f, url))?;

        // A whole-file answer starts at offset 0 (see check_response): its first `len` bytes
        // are the ones asked for, like with a range answer.
        let mut remaining = len;
        while remaining > 0 {
            let bytes = match tokio::time::timeout(self.timeout, response.chunk()).await {
                Err(_) => {
                    return Err(Failure::transient(format!(
                        "{url}: no data in time, {remaining} bytes missing"
                    )));
                }
                Ok(Err(e)) => return Err(Failure::transient(format!("{url}: {e:#}"))),
                Ok(Ok(None)) => {
                    return Err(Failure::Unusable(format!(
                        "{url}: the body ended {remaining} bytes early"
                    )));
                }
                Ok(Ok(Some(bytes))) => bytes,
            };
            let take = usize::try_from(remaining)
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            feeder.feed(&bytes[..take]).await?;
            remaining -= take as u64;
        }
        Ok(())
    }
}

fn with_url(failure: Failure, url: &str) -> Failure {
    match failure {
        Failure::Transient { error, retry_after } => Failure::Transient {
            error: format!("{url}: {error}"),
            retry_after,
        },
        Failure::FileUnavailable { file, error } => Failure::FileUnavailable {
            file,
            error: format!("{url}: {error}"),
        },
        Failure::Unusable(error) => Failure::Unusable(format!("{url}: {error}")),
        other => other,
    }
}

/// The URL of every file of the torrent on the web seed `base`; `None` for padding and empty
/// files. The paths are the ones the torrent publishes, whatever the files are called on disk;
/// like libtorrent, `name.utf-8` and `path.utf-8` win over `name` and `path` when present.
fn file_urls(
    base: &str,
    info: &ValidatedTorrentMetaV1Info<ByteBufOwned>,
    file_infos: &[FileInfo],
) -> Vec<Option<String>> {
    let meta = info.info();
    let name = meta
        .name_utf8
        .as_ref()
        .or(meta.name.as_ref())
        .map(|n| n.as_ref())
        .unwrap_or_default();
    // A single-file torrent maps its URL differently (BEP 19).
    let single = meta.length.is_some();
    let v1_files = meta.files.as_deref().unwrap_or_default();
    info.iter_file_details()
        .zip(file_infos)
        .enumerate()
        .map(|(index, (details, file))| {
            if file.attrs.padding || file.len == 0 {
                None
            } else if single {
                Some(web_seed_file_url(
                    base,
                    name,
                    None::<std::iter::Empty<&[u8]>>,
                ))
            } else if let Some(path) = v1_files.get(index).and_then(|f| f.path_utf8.as_ref()) {
                Some(web_seed_file_url(
                    base,
                    name,
                    Some(path.iter().map(|c| c.as_ref())),
                ))
            } else {
                Some(web_seed_file_url(
                    base,
                    name,
                    Some(details.filename.iter_components_bytes()),
                ))
            }
        })
        .collect()
}

/// The contiguous byte ranges (in torrent offsets) `chunks` cover, in order.
fn byte_segments(lengths: &Lengths, chunks: &[ChunkInfo]) -> Vec<(u64, u64)> {
    let mut segments: Vec<(u64, u64)> = Vec::new();
    for chunk in chunks {
        let start = lengths.chunk_absolute_offset(chunk);
        let end = start + u64::from(chunk.size);
        match segments.last_mut() {
            Some((_, last_end)) if *last_end == start => *last_end = end,
            _ => segments.push((start, end)),
        }
    }
    segments
}

/// Exponential backoff after `failures` transient failures in a row.
fn backoff(failures: u32) -> Duration {
    let factor = 1u32
        .checked_shl(failures.saturating_sub(1))
        .unwrap_or(u32::MAX);
    BACKOFF_FIRST.saturating_mul(factor).min(BACKOFF_MAX)
}

/// How long to wait after `failures` transient failures in a row, the last one with the
/// server asking to wait `retry_after`: the server may ask for a longer wait (up to
/// [`RETRY_AFTER_MAX`]), never for a shorter one.
fn transient_delay(retry_after: Option<Duration>, failures: u32) -> Duration {
    retry_after
        .unwrap_or_default()
        .min(RETRY_AFTER_MAX)
        .max(backoff(failures))
}

/// The delay in a `Retry-After` header. Only the delay-seconds form is understood; for an
/// HTTP date the computed backoff is used.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let secs: u64 = headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(secs))
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Parses `bytes first-last/total`; `total` is `None` for `*`.
fn parse_content_range(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let (range, total) = rest.split_once('/')?;
    let (first, last) = range.trim().split_once('-')?;
    let total = match total.trim() {
        "*" => None,
        t => Some(t.parse().ok()?),
    };
    Some((first.trim().parse().ok()?, last.trim().parse().ok()?, total))
}

/// Checks the head of the answer to a request for `len` bytes of file `file` (of `file_len`
/// bytes) from `offset`. `Ok` when the body can be read.
fn check_response(
    status: StatusCode,
    headers: &HeaderMap,
    file: usize,
    offset: u64,
    len: u64,
    file_len: u64,
) -> Result<(), Failure> {
    match status {
        StatusCode::PARTIAL_CONTENT => {
            if let Some(cl) = content_length(headers)
                && cl != len
            {
                return Err(Failure::Unusable(format!(
                    "asked for {len} bytes, the answer has {cl}"
                )));
            }
            if let Some(cr) = headers.get(CONTENT_RANGE) {
                let parsed = cr.to_str().ok().and_then(parse_content_range);
                let expected_last = offset + len - 1;
                match parsed {
                    Some((first, last, total))
                        if first == offset
                            && last == expected_last
                            && total.is_none_or(|t| t == file_len) => {}
                    _ => {
                        return Err(Failure::Unusable(format!(
                            "asked for bytes {offset}-{expected_last}/{file_len}, got {cr:?}"
                        )));
                    }
                }
            }
            Ok(())
        }
        // A server without range support: usable only for a request from the start of the
        // file, when the body is the whole file.
        StatusCode::OK => {
            if offset == 0 && content_length(headers) == Some(file_len) {
                Ok(())
            } else {
                Err(Failure::Unusable(
                    "the server does not answer range requests".to_owned(),
                ))
            }
        }
        StatusCode::NOT_FOUND
        | StatusCode::FORBIDDEN
        | StatusCode::GONE
        | StatusCode::RANGE_NOT_SATISFIABLE => Err(Failure::FileUnavailable {
            file,
            error: format!("HTTP {status}"),
        }),
        s if s.is_server_error()
            || s == StatusCode::TOO_MANY_REQUESTS
            || s == StatusCode::REQUEST_TIMEOUT =>
        {
            Err(Failure::Transient {
                error: format!("HTTP {status}"),
                retry_after: retry_after(headers),
            })
        }
        s => Err(Failure::Unusable(format!("unexpected HTTP {s}"))),
    }
}

/// Cuts the bytes of a run into its chunks and hands them, as they fill, to the common write
/// and verify path.
struct Feeder<'a> {
    state: &'a TorrentStateLive,
    seed: &'a WebSeed,
    counters: &'a AtomicPeerCounters,
    chunks: std::slice::Iter<'a, ChunkInfo>,
    /// The chunk being filled.
    current: Option<ChunkInfo>,
    buf: Vec<u8>,
    /// How many bytes of `buf` came from the web seed; the rest are the zeros of padding files,
    /// which are not asked for.
    received_in_buf: u32,
}

impl Feeder<'_> {
    async fn feed_zeros(&mut self, mut len: u64) -> Result<(), Failure> {
        while len > 0 {
            let n = usize::try_from(len).unwrap_or(usize::MAX).min(ZEROS.len());
            self.feed_bytes(&ZEROS[..n], false).await?;
            len -= n as u64;
        }
        Ok(())
    }

    async fn feed(&mut self, data: &[u8]) -> Result<(), Failure> {
        self.feed_bytes(data, true).await
    }

    async fn feed_bytes(&mut self, mut data: &[u8], received: bool) -> Result<(), Failure> {
        while !data.is_empty() {
            let Some(chunk) = self.current else {
                return Err(Failure::Local(anyhow::anyhow!(
                    "bug: web seed data past the reserved pieces"
                )));
            };
            let take = (chunk.size as usize - self.buf.len()).min(data.len());
            self.buf.extend_from_slice(&data[..take]);
            if received {
                // `take` is at most the chunk size, a u32.
                self.received_in_buf = self
                    .received_in_buf
                    .saturating_add(u32::try_from(take).unwrap_or(u32::MAX));
            }
            data = &data[take..];
            if self.buf.len() == chunk.size as usize {
                self.complete(&chunk).await?;
                self.buf.clear();
                self.received_in_buf = 0;
                self.current = self.chunks.next().copied();
            }
        }
        Ok(())
    }

    async fn complete(&self, chunk: &ChunkInfo) -> Result<(), Failure> {
        let received = self.received_in_buf;
        // Waiting here holds the body unread, so the server is slowed down too.
        self.state
            .wait_for_web_seed_download_rate(received)
            .await
            .context("waiting for the download rate limit")
            .map_err(Failure::Local)?;
        if !self
            .state
            .web_seed_owns(self.seed.handle(), chunk.piece_index)
        {
            trace!(url = self.seed.url(), piece = %chunk.piece_index, "web seed lost its piece");
            return Err(Failure::Lost);
        }
        self.counters
            .fetched_bytes
            .fetch_add(u64::from(received), std::sync::atomic::Ordering::Relaxed);
        self.counters
            .fetched_chunks
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let piece = Piece::from_data(chunk.piece_index.get(), chunk.offset, &self.buf[..]);
        let piece: Piece<ByteBuf<'_>> = piece;
        match self
            .state
            .process_received_chunk(
                self.seed.handle(),
                self.counters,
                &piece,
                chunk,
                u64::from(received),
            )
            .await
        {
            Ok(()) => {
                self.seed.add_downloaded(u64::from(received));
                Ok(())
            }
            Err(e) if e.downcast_ref::<PieceHashFailed>().is_some() => {
                // Chunks are written in order from the start of the piece, so the web seed
                // supplied all of it only when its last chunk completed it. Otherwise chunks
                // a peer wrote before the piece was stolen were part of it, and the failure
                // may be theirs: the piece is back in the queue and the web seed moves on.
                let piece_len = self.state.lengths().piece_length(chunk.piece_index);
                if chunk.offset + chunk.size >= piece_len {
                    Err(Failure::HashFailed(chunk.piece_index))
                } else {
                    debug!(
                        url = self.seed.url(),
                        piece = %chunk.piece_index,
                        "a piece completed with chunks from someone else failed the hash check"
                    );
                    Err(Failure::Lost)
                }
            }
            Err(e) => Err(Failure::Local(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    fn headers(pairs: &[(reqwest::header::HeaderName, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(k.clone(), HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn test_parse_content_range() {
        assert_eq!(
            parse_content_range("bytes 0-9/100"),
            Some((0, 9, Some(100)))
        );
        assert_eq!(parse_content_range("bytes 5-5/*"), Some((5, 5, None)));
        assert_eq!(
            parse_content_range(" bytes  1 - 2 / 3 "),
            Some((1, 2, Some(3)))
        );
        assert_eq!(parse_content_range("bytes */100"), None);
        assert_eq!(parse_content_range("items 0-9/100"), None);
        assert_eq!(parse_content_range("bytes 0-9"), None);
        assert_eq!(parse_content_range("bytes -1-9/100"), None);
    }

    #[test]
    fn test_check_response() {
        let ok = |s, h: &[(reqwest::header::HeaderName, &str)], offset, len| {
            check_response(s, &headers(h), 3, offset, len, 100)
        };
        assert!(ok(StatusCode::PARTIAL_CONTENT, &[], 10, 5).is_ok());
        assert!(
            ok(
                StatusCode::PARTIAL_CONTENT,
                &[(CONTENT_RANGE, "bytes 10-14/100"), (CONTENT_LENGTH, "5")],
                10,
                5
            )
            .is_ok()
        );
        assert!(
            ok(
                StatusCode::PARTIAL_CONTENT,
                &[(CONTENT_RANGE, "bytes 10-14/*")],
                10,
                5
            )
            .is_ok()
        );
        for bad in ["bytes 11-15/100", "bytes 10-14/99", "garbage"] {
            assert!(matches!(
                ok(StatusCode::PARTIAL_CONTENT, &[(CONTENT_RANGE, bad)], 10, 5),
                Err(Failure::Unusable(_))
            ));
        }
        assert!(matches!(
            ok(StatusCode::PARTIAL_CONTENT, &[(CONTENT_LENGTH, "6")], 10, 5),
            Err(Failure::Unusable(_))
        ));
        // Whole file: only from offset 0 and with the file's length.
        assert!(ok(StatusCode::OK, &[(CONTENT_LENGTH, "100")], 0, 5).is_ok());
        assert!(matches!(
            ok(StatusCode::OK, &[(CONTENT_LENGTH, "100")], 10, 5),
            Err(Failure::Unusable(_))
        ));
        assert!(matches!(
            ok(StatusCode::OK, &[], 0, 5),
            Err(Failure::Unusable(_))
        ));
        assert!(matches!(
            ok(StatusCode::OK, &[(CONTENT_LENGTH, "99")], 0, 5),
            Err(Failure::Unusable(_))
        ));
        for s in [404, 403, 410, 416] {
            assert!(matches!(
                ok(StatusCode::from_u16(s).unwrap(), &[], 0, 5),
                Err(Failure::FileUnavailable { file: 3, .. })
            ));
        }
        assert!(matches!(
            ok(StatusCode::SERVICE_UNAVAILABLE, &[(RETRY_AFTER, "7")], 0, 5),
            Err(Failure::Transient { retry_after: Some(d), .. }) if d == Duration::from_secs(7)
        ));
        assert!(matches!(
            ok(
                StatusCode::TOO_MANY_REQUESTS,
                &[(RETRY_AFTER, "Wed, 21 Oct 2015 07:28:00 GMT")],
                0,
                5
            ),
            Err(Failure::Transient {
                retry_after: None,
                ..
            })
        ));
        assert!(matches!(
            ok(StatusCode::UNAUTHORIZED, &[], 0, 5),
            Err(Failure::Unusable(_))
        ));
    }

    #[test]
    fn test_backoff() {
        assert_eq!(backoff(1), BACKOFF_FIRST);
        assert_eq!(backoff(2), BACKOFF_FIRST * 2);
        assert_eq!(backoff(3), BACKOFF_FIRST * 4);
        assert_eq!(backoff(20), BACKOFF_MAX);
        assert_eq!(backoff(u32::MAX), BACKOFF_MAX);
    }

    #[test]
    fn test_transient_delay() {
        // No wish: the back-off.
        assert_eq!(transient_delay(None, 1), BACKOFF_FIRST);
        assert_eq!(transient_delay(None, 3), BACKOFF_FIRST * 4);
        // A shorter wish, zero included, does not shorten it.
        assert_eq!(transient_delay(Some(Duration::ZERO), 1), BACKOFF_FIRST);
        assert_eq!(
            transient_delay(Some(Duration::from_secs(1)), 2),
            BACKOFF_FIRST * 2
        );
        // A longer one is honoured, up to a bound.
        assert_eq!(
            transient_delay(Some(Duration::from_secs(60)), 1),
            Duration::from_secs(60)
        );
        assert_eq!(
            transient_delay(Some(Duration::from_secs(u64::MAX)), 1),
            RETRY_AFTER_MAX
        );
    }

    /// `name.utf-8` and `path.utf-8` win over the legacy-encoded `name` and `path`, file by
    /// file; padding and empty files get no URL.
    #[test]
    fn test_file_urls_prefer_utf8_names() {
        fn bstr(s: &[u8]) -> Vec<u8> {
            let mut v = format!("{}:", s.len()).into_bytes();
            v.extend_from_slice(s);
            v
        }
        // "你" in GBK and in UTF-8.
        let gbk: &[u8] = b"\xc4\xe3";
        let utf8 = "你".as_bytes();
        let mut t = b"d4:infod5:filesl".to_vec();
        t.extend(b"d6:lengthi10e4:pathl");
        t.extend(bstr(gbk));
        t.extend(b"e10:path.utf-8l");
        t.extend(bstr(utf8));
        t.extend(b"ee");
        t.extend(b"d6:lengthi10e4:pathl3:");
        t.extend(gbk);
        t.extend(b"2ee");
        t.extend(b"d6:lengthi0e4:pathl1:eee");
        t.extend(b"e4:name");
        t.extend(bstr(gbk));
        t.extend(b"10:name.utf-8");
        t.extend(bstr(utf8));
        t.extend(b"12:piece lengthi16384e6:pieces20:");
        t.extend([0u8; 20]);
        t.extend(b"ee");
        use clone_to_owned::CloneToOwned;
        let info = librqbit_core::torrent_metainfo::torrent_from_bytes(&t)
            .unwrap()
            .info
            .data
            .clone_to_owned(None)
            .validate()
            .unwrap();
        assert_eq!(
            info.info().name_utf8.as_ref().map(|n| n.as_ref()),
            Some(utf8)
        );
        let file = |offset, len| FileInfo {
            relative_filename: "x".into(),
            offset_in_torrent: offset,
            piece_range: 0..1,
            attrs: Default::default(),
            len,
        };
        let urls = file_urls(
            "http://h/d/",
            &info,
            &[file(0, 10), file(10, 10), file(20, 0)],
        );
        assert_eq!(
            urls,
            vec![
                Some("http://h/d/%E4%BD%A0/%E4%BD%A0".to_owned()),
                Some("http://h/d/%E4%BD%A0/%C4%E32".to_owned()),
                None,
            ]
        );
    }

    #[test]
    fn test_byte_segments() {
        let lengths = Lengths::new(100_000, 32768).unwrap();
        let p = |i| lengths.validate_piece_index(i).unwrap();
        let chunks: Vec<_> = [p(0), p(1), p(3)]
            .iter()
            .flat_map(|i| lengths.iter_chunk_infos(*i))
            .collect();
        assert_eq!(
            byte_segments(&lengths, &chunks),
            vec![(0, 65536), (98304, 100_000)]
        );
    }
}
