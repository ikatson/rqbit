//! Creating torrents from files on disk: BitTorrent v1, v2 (BEP 52) and hybrid
//! torrents, with tracker tiers (BEP 12), web seeds (BEP 19), the private flag
//! (BEP 27), `source`, comment, creator and creation date.
//!
//! The content is read once: SHA-1 piece hashes (v1) and SHA-256 merkle trees
//! (v2) are computed in the same pass, file by file, with bounded memory.
//!
//! Files are ordered by path, comparing the UTF-8 bytes of each path component.
//! That is the order of the files in a bencoded v2 `file tree`, so the v1, v2
//! and hybrid torrents of a folder describe the same sequence of files, and the
//! output does not depend on the order the filesystem lists a folder in.

use std::fmt;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use bencode::{WithRawBytes, bencode_serialize_to_writer};
use buffers::ByteBufOwned;
use bytes::Bytes;
use librqbit_core::magnet::Magnet;
use librqbit_core::merkle::{self, BlockTreeHasher};
use librqbit_core::metainfo_v2::{PieceLayers, TorrentVersion, V2FileTree, V2FileTreeEntry};
use librqbit_core::torrent_metainfo::{
    TorrentMetaV1File, TorrentMetaV1Info, TorrentMetaV1Owned, compute_info_hashes,
    torrent_from_bytes,
};
use librqbit_core::{Id20, Id32, web_seed};
use sha1w::ISha1;
use tokio_util::sync::CancellationToken;

use crate::spawn_utils::BlockingSpawner;

/// Smallest piece length accepted: one BEP 52 block (16 KiB).
const MIN_PIECE_LENGTH: u32 = merkle::BLOCK_SIZE;
/// Largest piece length accepted (128 MiB), the bound libtorrent has.
const MAX_PIECE_LENGTH: u32 = 128 * 1024 * 1024;

/// How much of a file is read at once.
const READ_BUF_LEN: usize = 1024 * 1024;
/// How many bytes are hashed between two progress reports.
const PROGRESS_STEP: u64 = 8 * 1024 * 1024;

/// Zeros to hash as the padding of a hybrid torrent.
static ZEROS: [u8; 64 * 1024] = [0; 64 * 1024];

/// How far hashing has come, see [`CreateTorrentOptions::progress`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateTorrentProgress {
    /// Bytes of file content hashed so far (padding is not counted).
    pub hashed_bytes: u64,
    /// Bytes of file content in total.
    pub total_bytes: u64,
}

/// Receives [`CreateTorrentProgress`] reports, see [`CreateTorrentOptions::progress`].
pub type CreateTorrentProgressCallback = Arc<dyn Fn(CreateTorrentProgress) + Send + Sync>;

/// The error [`create_torrent`] returns when [`CreateTorrentOptions::cancel`]
/// was cancelled. Find it with `error.downcast_ref::<CreateTorrentCancelled>()`
/// or `error.is::<CreateTorrentCancelled>()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("torrent creation was cancelled")]
pub struct CreateTorrentCancelled;

/// What [`create_torrent`] makes. `CreateTorrentOptions::default()` is a v1
/// torrent, not private, without trackers or web seeds, with an automatic
/// piece length and `created by` set to rqbit's name and version.
#[derive(Clone)]
pub struct CreateTorrentOptions<'a> {
    /// The torrent's name: the file name of a single-file torrent, the folder
    /// of a multi-file one. Defaults to the last component of the path. Must
    /// not be empty or contain `/`.
    pub name: Option<&'a str>,
    /// Trackers, all in one tier. Ignored when `tracker_tiers` is not empty.
    pub trackers: Vec<String>,
    /// Tracker tiers (BEP 12), in order; used instead of `trackers` when not
    /// empty. Empty URLs and empty tiers are dropped. Like libtorrent, the
    /// torrent gets `announce` (the first tracker) and, when there is more than
    /// one tracker, `announce-list`.
    pub tracker_tiers: Vec<Vec<String>>,
    /// The piece length: a power of two from 16 KiB to 128 MiB. `None` picks
    /// one from the total size like libtorrent does (16 KiB up to about
    /// 2.5 MiB of content, doubling each time the size quadruples, at most
    /// 16 MiB), see [`auto_piece_length`].
    pub piece_length: Option<u32>,
    /// v1 (BEP 3, SHA-1 pieces), v2 (BEP 52, SHA-256 merkle trees) or hybrid
    /// (both; every file starts on a piece boundary thanks to BEP 47 padding
    /// files in the v1 file list).
    pub version: TorrentVersion,
    /// Sets `private` (BEP 27) in the info dictionary. Changes the info hash.
    pub private: bool,
    /// Web seeds (BEP 19 `url-list`): `http` / `https` URLs, at most 64, each
    /// at most 4096 bytes. Duplicates are dropped; an unusable URL is an error.
    pub web_seeds: Vec<String>,
    /// `source` in the info dictionary, which private trackers use to make the
    /// info hash differ from the same content's on other sites. An empty
    /// string is the same as `None`.
    pub source: Option<&'a str>,
    /// `comment`. An empty string is the same as `None`.
    pub comment: Option<&'a str>,
    /// `created by`. `None` writes `rqbit <version>`; an empty string writes
    /// nothing.
    pub created_by: Option<&'a str>,
    /// `creation date`, in seconds since the Unix epoch (not before it).
    /// `None` writes nothing, which keeps the output a function of the content
    /// and the options.
    pub creation_date: Option<i64>,
    /// Called with the hashing progress: once before the first byte, then
    /// every 8 MiB of content, then once with `hashed_bytes == total_bytes`.
    /// Runs on the thread that hashes, so it must return quickly.
    pub progress: Option<CreateTorrentProgressCallback>,
    /// Cancelling it stops the work between two reads; [`create_torrent`] then
    /// returns [`CreateTorrentCancelled`]. Nothing is written to disk either way.
    pub cancel: Option<CancellationToken>,
}

impl Default for CreateTorrentOptions<'_> {
    fn default() -> Self {
        Self {
            name: None,
            trackers: Vec::new(),
            tracker_tiers: Vec::new(),
            piece_length: None,
            version: TorrentVersion::V1,
            private: false,
            web_seeds: Vec::new(),
            source: None,
            comment: None,
            created_by: None,
            creation_date: None,
            progress: None,
            cancel: None,
        }
    }
}

impl fmt::Debug for CreateTorrentOptions<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CreateTorrentOptions")
            .field("name", &self.name)
            .field("trackers", &self.trackers)
            .field("tracker_tiers", &self.tracker_tiers)
            .field("piece_length", &self.piece_length)
            .field("version", &self.version)
            .field("private", &self.private)
            .field("web_seeds", &self.web_seeds)
            .field("source", &self.source)
            .field("comment", &self.comment)
            .field("created_by", &self.created_by)
            .field("creation_date", &self.creation_date)
            .field("progress", &self.progress.is_some())
            .field("cancel", &self.cancel)
            .finish()
    }
}

/// The piece length [`create_torrent`] picks for `total_bytes` of content
/// when none is given: libtorrent's table, which aims at a piece list of
/// about `2 * sqrt(total_bytes)` bytes.
///
/// | content up to | piece length |
/// |---|---|
/// | 2 684 355 B (~2.6 MiB) | 16 KiB |
/// | 10 737 418 B (~10 MiB) | 32 KiB |
/// | 42 949 673 B (~41 MiB) | 64 KiB |
/// | 171 798 692 B (~164 MiB) | 128 KiB |
/// | 687 194 767 B (~655 MiB) | 256 KiB |
/// | 2 748 779 069 B (~2.6 GiB) | 512 KiB |
/// | 10 995 116 278 B (~10 GiB) | 1 MiB |
/// | 43 980 465 111 B (~41 GiB) | 2 MiB |
/// | 175 921 860 444 B (~164 GiB) | 4 MiB |
/// | 703 687 441 777 B (~655 GiB) | 8 MiB |
/// | more | 16 MiB |
pub fn auto_piece_length(total_bytes: u64) -> u32 {
    const SIZE_TABLE: [u64; 10] = [
        2_684_355,
        10_737_418,
        42_949_673,
        171_798_692,
        687_194_767,
        2_748_779_069,
        10_995_116_278,
        43_980_465_111,
        175_921_860_444,
        703_687_441_777,
    ];
    let step = SIZE_TABLE
        .iter()
        .take_while(|&&limit| limit < total_bytes)
        .count();
    MIN_PIECE_LENGTH << step
}

fn validate_piece_length(piece_length: u32) -> anyhow::Result<()> {
    if !piece_length.is_power_of_two()
        || !(MIN_PIECE_LENGTH..=MAX_PIECE_LENGTH).contains(&piece_length)
    {
        bail!(
            "invalid piece length {piece_length}: it must be a power of two from {MIN_PIECE_LENGTH} (16 KiB) to {MAX_PIECE_LENGTH} (128 MiB)"
        );
    }
    Ok(())
}

/// A file of the torrent.
struct InputFile {
    /// Where its data is read from.
    path: PathBuf,
    /// Its path in the torrent: relative to the torrent's folder in a
    /// multi-file torrent, the torrent's name in a single-file one.
    components: Vec<String>,
    len: u64,
}

/// The files of the torrent, in torrent order.
struct Content {
    name: String,
    single_file: bool,
    files: Vec<InputFile>,
    output_folder: PathBuf,
    total_bytes: u64,
}

fn check_cancelled(cancel: Option<&CancellationToken>) -> anyhow::Result<()> {
    if cancel.is_some_and(|c| c.is_cancelled()) {
        return Err(CreateTorrentCancelled.into());
    }
    Ok(())
}

/// The files under `path` (or `path` itself if it is a file), in torrent order.
///
/// Regular files are included, empty ones too. A symbolic link to a file is
/// followed (the torrent has the target's data under the link's name); a link
/// to a directory is not followed, and its files are not part of the torrent.
/// Anything else (a broken link, a socket, a device), an unreadable directory
/// or a name that is not UTF-8 is an error naming the path.
fn collect_content(
    path: &Path,
    name: Option<&str>,
    cancel: Option<&CancellationToken>,
) -> anyhow::Result<Content> {
    let metadata = std::fs::metadata(path).with_context(|| format!("error reading {path:?}"))?;
    let absolute =
        std::path::absolute(path).with_context(|| format!("error resolving {path:?}"))?;
    let name = match name {
        Some(name) => name.to_owned(),
        None => {
            let basename = match absolute.file_name() {
                Some(n) => n.to_owned(),
                None => path
                    .canonicalize()
                    .with_context(|| format!("error resolving {path:?}"))?
                    .file_name()
                    .with_context(|| format!("{path:?} has no name, give the torrent one"))?
                    .to_owned(),
            };
            basename
                .to_str()
                .with_context(|| format!("the name of {path:?} is not valid UTF-8"))?
                .to_owned()
        }
    };
    if name.is_empty() || name.contains('/') {
        bail!("invalid torrent name {name:?}: it must not be empty or contain '/'");
    }

    if metadata.is_file() {
        let output_folder = absolute
            .parent()
            .with_context(|| format!("{path:?} has no parent folder"))?
            .to_owned();
        let len = metadata.len();
        return Ok(Content {
            files: vec![InputFile {
                path: path.to_owned(),
                components: vec![name.clone()],
                len,
            }],
            name,
            single_file: true,
            output_folder,
            total_bytes: len,
        });
    }
    if !metadata.is_dir() {
        bail!("{path:?} is neither a file nor a directory");
    }

    let mut files = Vec::new();
    let mut total_bytes = 0u64;
    for entry in walkdir::WalkDir::new(path).min_depth(1) {
        check_cancelled(cancel)?;
        let entry = entry.with_context(|| format!("error listing the files of {path:?}"))?;
        let file_type = entry.file_type();
        if file_type.is_dir() {
            continue;
        }
        let len = if file_type.is_file() {
            entry
                .metadata()
                .with_context(|| format!("error reading {:?}", entry.path()))?
                .len()
        } else if file_type.is_symlink() {
            let target = std::fs::metadata(entry.path())
                .with_context(|| format!("error following the link {:?}", entry.path()))?;
            if target.is_dir() {
                continue;
            }
            if !target.is_file() {
                bail!("{:?} does not link to a regular file", entry.path());
            }
            target.len()
        } else {
            bail!("{:?} is not a regular file", entry.path());
        };
        let relative = entry
            .path()
            .strip_prefix(path)
            .with_context(|| format!("{:?} is not under {path:?}", entry.path()))?;
        let components = relative
            .components()
            .map(|c| match c {
                Component::Normal(c) => c
                    .to_str()
                    .map(str::to_owned)
                    .with_context(|| format!("the name of {:?} is not valid UTF-8", entry.path())),
                _ => bail!("unexpected path {:?}", entry.path()),
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        total_bytes = total_bytes
            .checked_add(len)
            .context("the files are too large")?;
        files.push(InputFile {
            path: entry.into_path(),
            components,
            len,
        });
    }
    if files.is_empty() {
        bail!("{path:?} has no files");
    }
    sort_files(&mut files);
    Ok(Content {
        name,
        single_file: false,
        files,
        output_folder: path.to_owned(),
        total_bytes,
    })
}

/// Torrent order: by path, comparing the UTF-8 bytes of each component
/// (`str`'s order), which is how a bencoded `file tree` is sorted.
fn sort_files(files: &mut [InputFile]) {
    files.sort_unstable_by(|a, b| a.components.cmp(&b.components));
}

/// One entry of the v1 file list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum V1Entry {
    /// The file with this index in [`Content::files`].
    File(usize),
    /// A BEP 47 padding file of this length.
    Pad(u64),
}

/// The v1 file list of files of `lengths`, in order, and the length of the
/// data it describes. With `pad` (hybrid torrents), a padding file goes in
/// front of every non-empty file that would not start on a piece boundary, as
/// libtorrent does: so every file but the first non-empty one follows padding
/// unless the one before ends on a boundary, empty files take no padding, and
/// nothing pads the end.
fn v1_entries(
    lengths: impl IntoIterator<Item = u64>,
    piece_length: u32,
    pad: bool,
) -> anyhow::Result<(Vec<V1Entry>, u64)> {
    let pl = u64::from(piece_length);
    let mut offset = 0u64;
    let mut out = Vec::new();
    for (index, len) in lengths.into_iter().enumerate() {
        let misalignment = offset % pl;
        if pad && len > 0 && misalignment != 0 {
            let pad_len = pl - misalignment;
            out.push(V1Entry::Pad(pad_len));
            offset = offset.checked_add(pad_len).context("too much data")?;
        }
        out.push(V1Entry::File(index));
        offset = offset.checked_add(len).context("too much data")?;
    }
    Ok((out, offset))
}

/// SHA-1 hashes of the pieces of one stream of bytes.
struct V1Hasher {
    piece_length: u64,
    in_piece: u64,
    current: sha1w::Sha1,
    pieces: Vec<u8>,
}

impl V1Hasher {
    fn new(piece_length: u32) -> Self {
        Self {
            piece_length: u64::from(piece_length),
            in_piece: 0,
            current: sha1w::Sha1::new(),
            pieces: Vec::new(),
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            let room = self.piece_length - self.in_piece;
            let take = usize::try_from(room).map_or(data.len(), |room| room.min(data.len()));
            let (head, tail) = data.split_at(take);
            self.current.update(head);
            self.in_piece += head.len() as u64;
            data = tail;
            if self.in_piece == self.piece_length {
                let piece = std::mem::replace(&mut self.current, sha1w::Sha1::new());
                self.pieces.extend_from_slice(&piece.finish());
                self.in_piece = 0;
            }
        }
    }

    fn update_zeros(&mut self, mut len: u64) {
        while len > 0 {
            let take = usize::try_from(len).map_or(ZEROS.len(), |len| len.min(ZEROS.len()));
            self.update(&ZEROS[..take]);
            len -= take as u64;
        }
    }

    /// The piece hashes. A piece that was started is the last one; content
    /// that ends on a piece boundary has no extra empty piece (libtorrent
    /// rejects a torrent with more hashes than pieces).
    fn finish(mut self) -> Vec<u8> {
        if self.in_piece > 0 {
            self.pieces.extend_from_slice(&self.current.finish());
        }
        self.pieces
    }
}

/// The BEP 52 merkle tree of one non-empty file.
struct FileTreeHasher {
    piece_length: u64,
    /// The file fits in one piece: its root is the root over its own blocks,
    /// and it has no piece layer.
    small: bool,
    leaves: u64,
    current: BlockTreeHasher,
    in_piece: u64,
    layer: Vec<Id32>,
}

/// A file's `pieces root`, and its piece layer if it is larger than a piece.
type FileHashes = (Id32, Option<Vec<Id32>>);

impl FileTreeHasher {
    fn new(len: u64, piece_length: u32) -> anyhow::Result<Self> {
        let pl = u64::from(piece_length);
        let small = len <= pl;
        let leaves = if small {
            merkle::num_leafs(len).context("file too large")?
        } else {
            pl / u64::from(merkle::BLOCK_SIZE)
        };
        Ok(Self {
            piece_length: pl,
            small,
            leaves,
            current: BlockTreeHasher::new(),
            in_piece: 0,
            layer: Vec::new(),
        })
    }

    fn finish_piece(&mut self) -> anyhow::Result<()> {
        let piece = std::mem::take(&mut self.current);
        self.layer
            .push(piece.finish(self.leaves).context("piece larger than expected")?);
        self.in_piece = 0;
        Ok(())
    }

    fn update(&mut self, mut data: &[u8]) -> anyhow::Result<()> {
        if self.small {
            self.current.update(data);
            return Ok(());
        }
        while !data.is_empty() {
            let room = self.piece_length - self.in_piece;
            let take = usize::try_from(room).map_or(data.len(), |room| room.min(data.len()));
            let (head, tail) = data.split_at(take);
            self.current.update(head);
            self.in_piece += head.len() as u64;
            data = tail;
            if self.in_piece == self.piece_length {
                self.finish_piece()?;
            }
        }
        Ok(())
    }

    fn finish(mut self) -> anyhow::Result<FileHashes> {
        if self.small {
            let root = self
                .current
                .finish(self.leaves)
                .context("file larger than expected")?;
            return Ok((root, None));
        }
        if self.in_piece > 0 {
            self.finish_piece()?;
        }
        let piece_length = u32::try_from(self.piece_length).context("piece length")?;
        let root = merkle::pieces_root_from_piece_layer(&self.layer, piece_length)
            .context("error computing a pieces root")?;
        Ok((root, Some(self.layer)))
    }
}

/// Reports [`CreateTorrentProgress`] every [`PROGRESS_STEP`] bytes.
struct ProgressReporter<'a> {
    callback: Option<&'a CreateTorrentProgressCallback>,
    total_bytes: u64,
    hashed_bytes: u64,
    reported: u64,
}

impl ProgressReporter<'_> {
    fn report(&mut self) {
        self.reported = self.hashed_bytes;
        if let Some(callback) = self.callback {
            callback(CreateTorrentProgress {
                hashed_bytes: self.hashed_bytes,
                total_bytes: self.total_bytes,
            });
        }
    }

    fn advance(&mut self, bytes: u64) {
        self.hashed_bytes += bytes;
        if self.hashed_bytes - self.reported >= PROGRESS_STEP {
            self.report();
        }
    }
}

/// The hashes of the content.
struct Hashes {
    /// v1 piece hashes, concatenated (v1 and hybrid torrents).
    pieces: Option<Vec<u8>>,
    /// Per file, in torrent order: its root and piece layer, `None` for an
    /// empty file (v2 and hybrid torrents; empty otherwise).
    files: Vec<Option<FileHashes>>,
}

fn read_some(file: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    loop {
        match file.read(buf) {
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            r => return r,
        }
    }
}

/// Reads every file once, in the order of `entries`, and hashes it.
fn hash_content(
    content: &Content,
    entries: &[V1Entry],
    piece_length: u32,
    version: TorrentVersion,
    options: &CreateTorrentOptions<'_>,
) -> anyhow::Result<Hashes> {
    let cancel = options.cancel.as_ref();
    let mut v1 = version.has_v1().then(|| V1Hasher::new(piece_length));
    let mut files = Vec::with_capacity(if version.has_v2() {
        content.files.len()
    } else {
        0
    });
    let mut progress = ProgressReporter {
        callback: options.progress.as_ref(),
        total_bytes: content.total_bytes,
        hashed_bytes: 0,
        reported: 0,
    };
    progress.report();
    let mut buf = vec![0u8; READ_BUF_LEN];

    for entry in entries {
        let index = match *entry {
            V1Entry::Pad(len) => {
                if let Some(v1) = v1.as_mut() {
                    v1.update_zeros(len);
                }
                continue;
            }
            V1Entry::File(index) => index,
        };
        let input = &content.files[index];
        let path = &input.path;
        let mut tree = if version.has_v2() && input.len > 0 {
            Some(FileTreeHasher::new(input.len, piece_length)?)
        } else {
            None
        };
        let mut file = File::open(path).with_context(|| format!("error opening {path:?}"))?;
        let mut remaining = input.len;
        while remaining > 0 {
            check_cancelled(cancel)?;
            let want = usize::try_from(remaining).map_or(buf.len(), |r| r.min(buf.len()));
            let n = read_some(&mut file, &mut buf[..want])
                .with_context(|| format!("error reading {path:?}"))?;
            if n == 0 {
                bail!("{path:?} changed while it was being hashed: it got shorter");
            }
            let data = &buf[..n];
            if let Some(v1) = v1.as_mut() {
                v1.update(data);
            }
            if let Some(tree) = tree.as_mut() {
                tree.update(data)?;
            }
            remaining -= n as u64;
            progress.advance(n as u64);
        }
        if read_some(&mut file, &mut buf[..1]).with_context(|| format!("error reading {path:?}"))?
            != 0
        {
            bail!("{path:?} changed while it was being hashed: it got longer");
        }
        if version.has_v2() {
            files.push(tree.map(FileTreeHasher::finish).transpose()?);
        }
    }
    progress.report();
    Ok(Hashes {
        pieces: v1.map(V1Hasher::finish),
        files,
    })
}

fn bytes_of(s: &str) -> ByteBufOwned {
    ByteBufOwned::from(s.as_bytes())
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.filter(|s| !s.is_empty())
}

/// The tracker tiers of `options`, without empty URLs and tiers.
fn tracker_tiers(options: &CreateTorrentOptions<'_>) -> Vec<Vec<String>> {
    let tiers = if options.tracker_tiers.is_empty() {
        std::slice::from_ref(&options.trackers)
    } else {
        options.tracker_tiers.as_slice()
    };
    tiers
        .iter()
        .map(|tier| {
            tier.iter()
                .filter(|url| !url.is_empty())
                .cloned()
                .collect::<Vec<_>>()
        })
        .filter(|tier| !tier.is_empty())
        .collect()
}

fn web_seeds(options: &CreateTorrentOptions<'_>) -> anyhow::Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for url in options.web_seeds.iter().map(|url| url.trim()) {
        if !web_seed::is_web_seed_url(url) {
            bail!(
                "invalid web seed {url:?}: it must be an http or https URL with a host, at most {} bytes long",
                web_seed::MAX_WEB_SEED_URL_LEN
            );
        }
        if out.iter().any(|u| u == url) {
            continue;
        }
        if out.len() == web_seed::MAX_WEB_SEEDS {
            bail!("too many web seeds, at most {} are allowed", web_seed::MAX_WEB_SEEDS);
        }
        out.push(url.to_owned());
    }
    Ok(out)
}

/// The info dictionary.
fn build_info(
    content: &Content,
    entries: &[V1Entry],
    piece_length: u32,
    version: TorrentVersion,
    options: &CreateTorrentOptions<'_>,
    hashes: &Hashes,
) -> anyhow::Result<TorrentMetaV1Info<ByteBufOwned>> {
    let path_of = |file: &InputFile| file.components.iter().map(|c| bytes_of(c)).collect();
    let (length, files) = if !version.has_v1() {
        (None, None)
    } else if content.single_file {
        (Some(content.total_bytes), None)
    } else {
        let files = entries
            .iter()
            .map(|entry| match *entry {
                V1Entry::File(index) => {
                    let file = &content.files[index];
                    TorrentMetaV1File {
                        length: file.len,
                        path: path_of(file),
                        path_utf8: None,
                        attr: None,
                        sha1: None,
                        symlink_path: None,
                    }
                }
                V1Entry::Pad(len) => TorrentMetaV1File {
                    length: len,
                    path: vec![bytes_of(".pad"), bytes_of(&len.to_string())],
                    path_utf8: None,
                    attr: Some(bytes_of("p")),
                    sha1: None,
                    symlink_path: None,
                },
            })
            .collect();
        (None, Some(files))
    };
    let file_tree = if version.has_v2() {
        let tree = content
            .files
            .iter()
            .zip(&hashes.files)
            .map(|(file, hashes)| V2FileTreeEntry {
                path: path_of(file),
                length: file.len,
                pieces_root: hashes
                    .as_ref()
                    .map(|(root, _)| ByteBufOwned(Bytes::copy_from_slice(&root.0))),
                attr: None,
            })
            .collect();
        Some(V2FileTree::new(tree))
    } else {
        None
    };
    Ok(TorrentMetaV1Info {
        name: Some(bytes_of(&content.name)),
        name_utf8: None,
        pieces: hashes.pieces.clone().map(ByteBufOwned::from),
        piece_length,
        length,
        attr: None,
        sha1: None,
        symlink_path: None,
        md5sum: None,
        files,
        private: options.private,
        source: non_empty(options.source).map(bytes_of),
        meta_version: version.has_v2().then_some(2),
        file_tree,
    })
}

/// The `piece layers` of a v2 torrent: one entry per distinct pieces root of
/// the files larger than a piece.
fn build_piece_layers(hashes: &Hashes) -> PieceLayers<ByteBufOwned> {
    let mut out: Vec<(ByteBufOwned, ByteBufOwned)> = Vec::new();
    for (root, layer) in hashes.files.iter().flatten() {
        let Some(layer) = layer else { continue };
        if out.iter().any(|(r, _)| r.as_ref() == root.0.as_slice()) {
            continue;
        }
        let mut bytes = Vec::with_capacity(layer.len() * 32);
        for hash in layer {
            bytes.extend_from_slice(&hash.0);
        }
        out.push((
            ByteBufOwned(Bytes::copy_from_slice(&root.0)),
            ByteBufOwned::from(bytes),
        ));
    }
    PieceLayers(out)
}

fn create_torrent_blocking(
    path: &Path,
    options: &CreateTorrentOptions<'_>,
) -> anyhow::Result<CreateTorrentResult> {
    let cancel = options.cancel.as_ref();
    check_cancelled(cancel)?;
    if let Some(piece_length) = options.piece_length {
        validate_piece_length(piece_length)?;
    }
    let tiers = tracker_tiers(options);
    let url_list = web_seeds(options)?;
    let creation_date = options
        .creation_date
        .map(usize::try_from)
        .transpose()
        .context("invalid creation date: it must not be before 1970")?;
    let version = options.version;

    let content = collect_content(path, options.name, cancel)?;
    if content.total_bytes == 0 {
        bail!("{path:?} has no data: every file is empty");
    }
    let piece_length = options
        .piece_length
        .unwrap_or_else(|| auto_piece_length(content.total_bytes));
    let (entries, stream_len) = v1_entries(
        content.files.iter().map(|f| f.len),
        piece_length,
        version == TorrentVersion::Hybrid,
    )?;
    if stream_len.div_ceil(u64::from(piece_length)) > u64::from(u32::MAX) {
        bail!("too many pieces: use a larger piece length");
    }

    let hashes = hash_content(&content, &entries, piece_length, version, options)?;
    let info = build_info(&content, &entries, piece_length, version, options, &hashes)?;
    let mut raw_info = Vec::new();
    bencode_serialize_to_writer(&info, &mut raw_info).context("error serializing the info")?;
    let (info_hash, info_hash_v2) = compute_info_hashes(&info, &raw_info);

    let announce = tiers.first().and_then(|t| t.first()).map(|t| bytes_of(t));
    let announce_list = if tiers.iter().map(Vec::len).sum::<usize>() > 1 {
        tiers
            .iter()
            .map(|tier| tier.iter().map(|t| bytes_of(t)).collect())
            .collect()
    } else {
        Vec::new()
    };
    let created_by = match options.created_by {
        None => Some(bytes_of(crate::client_name_and_version())),
        Some(by) => non_empty(Some(by)).map(bytes_of),
    };
    let result = CreateTorrentResult {
        meta: TorrentMetaV1Owned {
            announce,
            announce_list,
            info: WithRawBytes {
                data: info,
                raw_bytes: ByteBufOwned::from(raw_info),
            },
            comment: non_empty(options.comment).map(bytes_of),
            created_by,
            encoding: Some(bytes_of("utf-8")),
            publisher: None,
            publisher_url: None,
            creation_date,
            piece_layers: version.has_v2().then(|| build_piece_layers(&hashes)),
            url_list,
            info_hash,
            info_hash_v2,
        },
        output_folder: content.output_folder,
    };

    // Never hand out a torrent that rqbit itself would refuse to load.
    let bytes = result.as_bytes()?;
    let parsed = torrent_from_bytes(&bytes).context("the created torrent does not parse")?;
    parsed
        .validate_with_piece_layers()
        .context("the created torrent is not valid")?;
    if (parsed.info_hash, parsed.info_hash_v2) != (info_hash, info_hash_v2) {
        bail!("the created torrent parses with other info hashes");
    }
    Ok(result)
}

/// A torrent made by [`create_torrent`].
#[derive(Debug)]
pub struct CreateTorrentResult {
    pub meta: TorrentMetaV1Owned,
    /// Where the data is: the folder of a multi-file torrent's files, the
    /// folder that has the file of a single-file torrent.
    pub output_folder: PathBuf,
}

impl CreateTorrentResult {
    pub fn as_info(&self) -> &TorrentMetaV1Owned {
        &self.meta
    }

    /// The torrent's identifying hash: the v1 info hash (SHA-1) of a v1 or
    /// hybrid torrent, the v2 info hash truncated to 20 bytes for a v2 one.
    pub fn info_hash(&self) -> Id20 {
        self.meta.info_hash
    }

    /// The v2 info hash (SHA-256) of a v2 or hybrid torrent.
    pub fn info_hash_v2(&self) -> Option<Id32> {
        self.meta.info_hash_v2
    }

    /// A magnet link: `xt=urn:btih:` for v1 and hybrid torrents,
    /// `xt=urn:btmh:1220…` for v2 and hybrid ones, the trackers in tier order
    /// (`tr=`) and the web seeds (`ws=`).
    pub fn as_magnet(&self) -> Magnet {
        let trackers: Vec<String> = self
            .meta
            .iter_announce()
            .map(|t| String::from_utf8_lossy(t.as_ref()).into_owned())
            .collect();
        let v2 = self.info_hash_v2();
        let has_v1 = self.meta.info.data.pieces.is_some();
        // A v2-only torrent is known by its v2 hash only: the truncated hash
        // in `info_hash()` is not a v1 info hash.
        let v1 = (has_v1 || v2.is_none()).then_some(self.info_hash());
        // `new` fails only when there is no hash at all, and `v1` is set
        // whenever `v2` is not.
        let mut magnet = Magnet::new(v1, v2, trackers.clone(), None)
            .unwrap_or_else(|_| Magnet::from_id20(self.info_hash(), trackers, None));
        magnet.name = self
            .meta
            .info
            .data
            .name
            .as_ref()
            .map(|n| String::from_utf8_lossy(n.as_ref()).into_owned());
        magnet.web_seeds = self.meta.url_list.clone();
        magnet
    }

    /// The .torrent file.
    pub fn as_bytes(&self) -> anyhow::Result<Bytes> {
        let mut b = Vec::new();
        bencode_serialize_to_writer(&self.meta, &mut b).context("error serializing torrent")?;
        Ok(b.into())
    }
}

/// Creates a torrent of the file or folder at `path` (see the module
/// documentation for the file order and [`CreateTorrentOptions`]).
///
/// The files are listed and read on a blocking thread from `spawner` (inline
/// on a current-thread runtime), once. Errors name the file at fault;
/// cancellation returns [`CreateTorrentCancelled`]. Nothing is written to disk.
pub async fn create_torrent<'a>(
    path: &'a Path,
    options: CreateTorrentOptions<'a>,
    spawner: &BlockingSpawner,
) -> anyhow::Result<CreateTorrentResult> {
    spawner
        .block_in_place_with_semaphore(|| create_torrent_blocking(path, &options))
        .await
}

#[cfg(test)]
mod tests {
    use librqbit_core::torrent_metainfo::torrent_from_bytes;

    use crate::{CreateTorrentOptions, create_torrent, spawn_utils::BlockingSpawner};

    #[tokio::test]
    async fn test_create_torrent() {
        use crate::tests::test_util;

        let dir = test_util::create_default_random_dir_with_torrents(
            3,
            1000 * 1000,
            Some("rqbit_test_create_torrent"),
        );
        let torrent = create_torrent(dir.path(), Default::default(), &BlockingSpawner::new(1))
            .await
            .unwrap();

        let bytes = torrent.as_bytes().unwrap();

        let deserialized = torrent_from_bytes(&bytes).unwrap();
        assert_eq!(torrent.info_hash(), deserialized.info_hash);
    }

    async fn piece_count(files: &[(&str, usize)], piece_length: u32) -> usize {
        let dir = tempfile::tempdir().unwrap();
        for (name, len) in files {
            std::fs::write(dir.path().join(name), vec![7u8; *len]).unwrap();
        }
        let torrent = create_torrent(
            dir.path(),
            CreateTorrentOptions {
                piece_length: Some(piece_length),
                ..Default::default()
            },
            &BlockingSpawner::new(1),
        )
        .await
        .unwrap();
        torrent
            .meta
            .info
            .data
            .pieces
            .as_ref()
            .unwrap()
            .as_ref()
            .len()
            / 20
    }

    const P: usize = 16 * 1024;

    // libtorrent rejects a torrent that has more piece hashes than pieces.
    #[tokio::test]
    async fn test_content_ending_on_a_piece_boundary_has_no_extra_piece() {
        assert_eq!(piece_count(&[("a", 4 * P)], P as u32).await, 4);
        assert_eq!(piece_count(&[("a", 4 * P + 1)], P as u32).await, 5);
        assert_eq!(piece_count(&[("a", 1)], P as u32).await, 1);
    }

    #[tokio::test]
    async fn test_empty_file_does_not_drop_the_last_piece() {
        // Whatever order the two files are read in, there are 1.5 pieces of data: two pieces.
        assert_eq!(
            piece_count(&[("a", P + P / 2), ("b", 0)], P as u32).await,
            2
        );
    }
}
