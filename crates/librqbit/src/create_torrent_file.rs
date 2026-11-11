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
/// data it describes. With `pad` (hybrid torrents) and more than one file, a
/// padding file follows every non-empty file that does not end on a piece
/// boundary, the last one too, so that every file starts on a boundary. That is
/// what libtorrent writes by default, so the same folder makes the same hybrid
/// torrent. A lone file is never padded: there is nothing to align after it.
fn v1_entries(
    lengths: impl IntoIterator<Item = u64, IntoIter: ExactSizeIterator>,
    piece_length: u32,
    pad: bool,
) -> anyhow::Result<(Vec<V1Entry>, u64)> {
    let pl = u64::from(piece_length);
    let lengths = lengths.into_iter();
    let pad = pad && lengths.len() > 1;
    let mut offset = 0u64;
    let mut out = Vec::with_capacity(if pad {
        2 * lengths.len()
    } else {
        lengths.len()
    });
    for (index, len) in lengths.enumerate() {
        out.push(V1Entry::File(index));
        offset = offset.checked_add(len).context("too much data")?;
        let misalignment = offset % pl;
        if pad && len > 0 && misalignment != 0 {
            let pad_len = pl - misalignment;
            out.push(V1Entry::Pad(pad_len));
            offset = offset.checked_add(pad_len).context("too much data")?;
        }
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
        self.layer.push(
            piece
                .finish(self.leaves)
                .context("piece larger than expected")?,
        );
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
            bail!(
                "too many web seeds, at most {} are allowed",
                web_seed::MAX_WEB_SEEDS
            );
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
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;
    use librqbit_core::magnet::Magnet;
    use librqbit_core::metainfo_v2::TorrentVersion;
    use librqbit_core::torrent_metainfo::torrent_from_bytes;
    use tokio_util::sync::CancellationToken;

    use super::{
        CreateTorrentCancelled, CreateTorrentOptions, CreateTorrentProgress,
        CreateTorrentProgressCallback, CreateTorrentResult, InputFile, MAX_PIECE_LENGTH,
        MIN_PIECE_LENGTH, PROGRESS_STEP, auto_piece_length, create_torrent, sort_files, v1_entries,
        validate_piece_length,
    };
    use crate::spawn_utils::BlockingSpawner;

    const P: u32 = 16 * 1024;
    const PL: usize = P as usize;
    const FORMATS: [TorrentVersion; 3] = [
        TorrentVersion::V1,
        TorrentVersion::V2,
        TorrentVersion::Hybrid,
    ];

    /// Content sizes around the steps of the automatic piece length, and the piece length
    /// picked for each.
    pub(crate) const AUTO_PIECE_LENGTHS: &[(u64, u32)] = &[
        (1, 16 << 10),
        (2_684_355, 16 << 10),
        (2_684_356, 32 << 10),
        (10_737_418, 32 << 10),
        (10_737_419, 64 << 10),
        (42_949_673, 64 << 10),
        (42_949_674, 128 << 10),
        (171_798_692, 128 << 10),
        (171_798_693, 256 << 10),
        (687_194_767, 256 << 10),
        (687_194_768, 512 << 10),
        (2_748_779_069, 512 << 10),
        (2_748_779_070, 1 << 20),
        (10_995_116_278, 1 << 20),
        (10_995_116_279, 2 << 20),
        (43_980_465_111, 2 << 20),
        (43_980_465_112, 4 << 20),
        (175_921_860_444, 4 << 20),
        (175_921_860_445, 8 << 20),
        (703_687_441_777, 8 << 20),
        (703_687_441_778, 16 << 20),
        (10_000_000_000_000, 16 << 20),
    ];

    /// A file of a test: its path in the torrent ('/'-separated) and its content.
    type TestFile = (String, Vec<u8>);

    /// Pseudo-random bytes, different for every `seed`, so that no two pieces are alike.
    fn data(seed: u64, len: usize) -> Vec<u8> {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x.to_le_bytes()[0]
            })
            .collect()
    }

    fn files<S: AsRef<str>>(spec: &[(S, usize)]) -> Vec<TestFile> {
        spec.iter()
            .zip(1..)
            .map(|((path, len), seed)| (path.as_ref().to_owned(), data(seed, *len)))
            .collect()
    }

    fn write_files<'a>(root: &Path, files: impl IntoIterator<Item = &'a TestFile>) {
        for (path, content) in files {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, content).unwrap();
        }
    }

    fn tempdir() -> tempfile::TempDir {
        tempfile::TempDir::with_prefix("rqbit_create_torrent").unwrap()
    }

    fn folder_with(files: &[TestFile]) -> tempfile::TempDir {
        let dir = tempdir();
        write_files(dir.path(), files);
        dir
    }

    async fn create(
        path: &Path,
        options: CreateTorrentOptions<'_>,
    ) -> anyhow::Result<CreateTorrentResult> {
        create_torrent(path, options, &BlockingSpawner::new(1)).await
    }

    async fn torrent_bytes(path: &Path, options: CreateTorrentOptions<'_>) -> Bytes {
        create(path, options).await.unwrap().as_bytes().unwrap()
    }

    async fn error_of(path: &Path, options: CreateTorrentOptions<'_>) -> String {
        format!("{:#}", create(path, options).await.unwrap_err())
    }

    fn options(version: TorrentVersion) -> CreateTorrentOptions<'static> {
        CreateTorrentOptions {
            version,
            piece_length: Some(P),
            ..Default::default()
        }
    }

    fn components(path: &str) -> Vec<String> {
        path.split('/').map(str::to_owned).collect()
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    fn text<B: AsRef<[u8]>>(b: Option<&B>) -> Option<&str> {
        b.map(|b| std::str::from_utf8(b.as_ref()).unwrap())
    }

    /// Checks `torrent` against the content it was made of: it parses back with the same info
    /// hashes and format, lists `files` in this order (padding files aside), and every piece
    /// verifies against the data, and does not with a byte of it changed.
    fn check(torrent: &CreateTorrentResult, version: TorrentVersion, files: &[TestFile]) {
        let bytes = torrent.as_bytes().unwrap();
        let parsed = torrent_from_bytes(&bytes).unwrap();
        assert_eq!(parsed.info_hash, torrent.info_hash());
        assert_eq!(parsed.info_hash_v2, torrent.info_hash_v2());
        assert_eq!(torrent.info_hash_v2().is_some(), version.has_v2());
        let info = parsed.validate_with_piece_layers().unwrap();
        assert_eq!(info.version(), version);

        let listed: Vec<(Vec<String>, u64)> = info
            .iter_file_details()
            .filter(|d| !d.attrs().padding)
            .map(|d| (d.filename.to_vec(), d.len))
            .collect();
        let expected: Vec<(Vec<String>, u64)> = files
            .iter()
            .map(|(path, content)| (components(path), content.len() as u64))
            .collect();
        assert_eq!(listed, expected);

        // The pieces as one stream of bytes: padding files, and in a v2 torrent the gap from
        // the end of a file to the next piece boundary, read as zeros.
        let content: HashMap<Vec<String>, &[u8]> = files
            .iter()
            .map(|(path, content)| (components(path), content.as_slice()))
            .collect();
        let mut stream = Vec::new();
        for file in info.iter_file_details_ext() {
            let start = usize::try_from(file.offset).unwrap();
            let end = start + usize::try_from(file.details.len).unwrap();
            if stream.len() < end {
                stream.resize(end, 0);
            }
            if !file.details.attrs().padding {
                stream[start..end].copy_from_slice(content[&file.details.filename.to_vec()]);
            }
        }
        let lengths = info.lengths();
        let mut pieces = 0;
        for piece in lengths.iter_piece_infos() {
            let index = piece.piece_index;
            let start = usize::try_from(lengths.piece_offset(index)).unwrap();
            let data = &stream[start..start + piece.len as usize];
            let mut verifier = info.piece_verifier(index).expect("no hash for a piece");
            assert_eq!(verifier.checks_v1(), version.has_v1());
            assert_eq!(verifier.checks_v2(), version.has_v2());
            verifier.update(data);
            assert!(verifier.finish(), "piece {} does not verify", index.get());

            // A piece starts with file data in every format.
            let mut changed = data.to_vec();
            changed[0] ^= 1;
            let mut verifier = info.piece_verifier(index).unwrap();
            verifier.update(&changed);
            assert!(!verifier.finish(), "piece {} verifies changed", index.get());
            pieces += 1;
        }
        assert_eq!(pieces, lengths.total_pieces());
    }

    /// The folders of the layout tests, with their files in torrent order.
    fn folder_cases() -> Vec<(&'static str, Vec<TestFile>)> {
        let many: Vec<(String, usize)> = (0..150)
            .map(|i| (format!("many/{i:03}.txt"), (i * 37) % 900 + 1))
            .collect();
        vec![
            (
                "nested folders",
                // "a/..." sorts before "a.b": paths are compared component by component.
                files(&[
                    ("a/b/c/deep.bin", 2 * PL + 5),
                    ("a/x.bin", 700),
                    ("a.b", 300),
                    ("top.bin", PL + 1),
                ]),
            ),
            (
                "exact multiple of the piece length",
                files(&[("exact.bin", 4 * PL), ("next.bin", 2 * PL)]),
            ),
            (
                "smaller than a piece",
                files(&[("big.bin", 3 * PL + 3), ("small.bin", 100)]),
            ),
            ("empty first", files(&[("a", 0), ("b", 1500), ("c", 1500)])),
            ("empty middle", files(&[("a", 1500), ("b", 0), ("c", 1500)])),
            ("empty last", files(&[("a", 1500), ("b", 1500), ("c", 0)])),
            ("many small files", files(&many)),
            (
                "non-ASCII names",
                // By UTF-8 bytes: 'B' < 'z' < 'Ü' (C3 9C) < 'ä' (C3 A4) < 'é' (C3 A9).
                files(&[
                    ("B.txt", 10),
                    ("z.txt", 10),
                    ("Ünïcødé/É 本.bin", 3000),
                    ("ä.txt", 2 * PL),
                    ("émoji 🎉.dat", 20_000),
                ]),
            ),
        ]
    }

    #[tokio::test]
    async fn test_folder_layouts_in_every_format() {
        for (case, files) in folder_cases() {
            let dir = folder_with(&files);
            for version in FORMATS {
                eprintln!("{case}, {version:?}");
                let torrent = create(dir.path(), options(version)).await.unwrap();
                check(&torrent, version, &files);
                assert_eq!(torrent.output_folder, dir.path());
            }
        }
    }

    #[tokio::test]
    async fn test_single_file_in_every_format() {
        for len in [1, 5000, PL, 2 * PL, 3 * PL + 123] {
            let dir = tempdir();
            let path = dir.path().join("single.bin");
            let content = data(len as u64, len);
            std::fs::write(&path, &content).unwrap();
            let files = vec![("single.bin".to_owned(), content)];
            for version in FORMATS {
                eprintln!("{len} bytes, {version:?}");
                let torrent = create(&path, options(version)).await.unwrap();
                check(&torrent, version, &files);
                // `length` instead of a file list, so never padding.
                let info = &torrent.meta.info.data;
                assert!(info.files.is_none());
                assert_eq!(info.length, version.has_v1().then_some(len as u64));
                assert_eq!(torrent.output_folder, dir.path());
            }
        }
    }

    #[tokio::test]
    async fn test_create_torrent() {
        use crate::tests::test_util;

        let dir = test_util::create_default_random_dir_with_torrents(
            3,
            1000 * 1000,
            Some("rqbit_test_create_torrent"),
        );
        let torrent = create(dir.path(), Default::default()).await.unwrap();
        let bytes = torrent.as_bytes().unwrap();
        let deserialized = torrent_from_bytes(&bytes).unwrap();
        assert_eq!(torrent.info_hash(), deserialized.info_hash);
    }

    #[tokio::test]
    async fn test_output_is_deterministic() {
        let files = files(&[
            ("b/1", 3000),
            ("a", 2 * PL + 1),
            ("c/d/e", 1),
            ("b/0", 0),
            ("é", 5),
        ]);
        let forward = folder_with(&files);
        let backward = tempdir();
        write_files(backward.path(), files.iter().rev());
        for version in FORMATS {
            let options = || CreateTorrentOptions {
                name: Some("same"),
                ..options(version)
            };
            let first = torrent_bytes(forward.path(), options()).await;
            assert_eq!(first, torrent_bytes(forward.path(), options()).await);
            assert_eq!(first, torrent_bytes(backward.path(), options()).await);
        }
    }

    #[test]
    fn test_file_order_does_not_depend_on_the_listing_order() {
        let expected = ["B", "a/a", "a/b", "a.b", "z", "Ü/x", "é"];
        for rotation in 0..expected.len() {
            for reverse in [false, true] {
                let mut listing = expected.to_vec();
                listing.rotate_left(rotation);
                if reverse {
                    listing.reverse();
                }
                let mut files: Vec<InputFile> = listing
                    .iter()
                    .map(|path| InputFile {
                        path: PathBuf::from(path),
                        components: components(path),
                        len: 1,
                    })
                    .collect();
                sort_files(&mut files);
                let sorted: Vec<String> = files.iter().map(|f| f.components.join("/")).collect();
                assert_eq!(sorted, expected);
            }
        }
    }

    #[tokio::test]
    async fn test_fields_round_trip() {
        let files = files(&[("a/one.bin", 2 * PL + 7), ("two.bin", 900)]);
        let dir = folder_with(&files);
        let name = dir.path().file_name().unwrap().to_str().unwrap().to_owned();
        let t1 = "udp://t1.example:1337/announce";
        let t2 = "http://t2.example/announce";
        let t3 = "https://t3.example/announce";
        let ws1 = "http://ws1.example/files/";
        let ws2 = "https://ws2.example/x";
        for version in FORMATS {
            let torrent = create(
                dir.path(),
                CreateTorrentOptions {
                    trackers: vec!["http://ignored.example/announce".to_owned()],
                    tracker_tiers: vec![
                        vec![t1.to_owned(), String::new(), t2.to_owned()],
                        vec![],
                        vec![t3.to_owned()],
                    ],
                    web_seeds: vec![ws1.to_owned(), format!(" {ws2} "), ws1.to_owned()],
                    private: true,
                    source: Some("SITE"),
                    comment: Some("a comment"),
                    created_by: Some("tester 1.0"),
                    creation_date: Some(1_700_000_000),
                    ..options(version)
                },
            )
            .await
            .unwrap();
            check(&torrent, version, &files);

            let bytes = torrent.as_bytes().unwrap();
            let parsed = torrent_from_bytes(&bytes).unwrap();
            assert_eq!(text(parsed.announce.as_ref()), Some(t1));
            let tiers: Vec<Vec<&str>> = parsed
                .announce_list
                .iter()
                .map(|tier| tier.iter().map(|url| text(Some(url)).unwrap()).collect())
                .collect();
            assert_eq!(tiers, [vec![t1, t2], vec![t3]]);
            assert_eq!(parsed.url_list, [ws1, ws2]);
            assert!(parsed.info.data.private);
            assert!(contains(parsed.info.raw_bytes.as_ref(), b"7:privatei1e"));
            assert_eq!(text(parsed.info.data.source.as_ref()), Some("SITE"));
            assert_eq!(text(parsed.comment.as_ref()), Some("a comment"));
            assert_eq!(text(parsed.created_by.as_ref()), Some("tester 1.0"));
            assert_eq!(parsed.creation_date, Some(1_700_000_000));

            let magnet = torrent.as_magnet().to_string();
            assert_eq!(magnet.contains("xt=urn:btih:"), version.has_v1());
            assert_eq!(magnet.contains("xt=urn:btmh:1220"), version.has_v2());
            let magnet = Magnet::parse(&magnet).unwrap();
            assert_eq!(
                magnet.as_id20(),
                version.has_v1().then_some(torrent.info_hash())
            );
            assert_eq!(magnet.as_id32(), torrent.info_hash_v2());
            assert_eq!(magnet.trackers, [t1, t2, t3]);
            assert_eq!(magnet.web_seeds, [ws1, ws2]);
            // The magnet string has no `dn`, the `Magnet` has the name.
            assert_eq!(magnet.name, None);
            assert_eq!(torrent.as_magnet().name.as_deref(), Some(name.as_str()));
        }
    }

    #[tokio::test]
    async fn test_private_and_source_change_the_info_hash() {
        let dir = folder_with(&files(&[("a", 3000)]));
        let hashes = |t: &CreateTorrentResult| (t.info_hash(), t.info_hash_v2());
        for version in FORMATS {
            let plain = create(dir.path(), options(version)).await.unwrap();
            // Like libtorrent: no `private` key unless private, no `source` unless given.
            let raw = plain.meta.info.raw_bytes.as_ref();
            assert!(!contains(raw, b"7:private"));
            assert!(!contains(raw, b"6:source"));
            let private = CreateTorrentOptions {
                private: true,
                ..options(version)
            };
            let private = create(dir.path(), private).await.unwrap();
            let source = CreateTorrentOptions {
                source: Some("SITE"),
                ..options(version)
            };
            let source = create(dir.path(), source).await.unwrap();
            let empty_source = CreateTorrentOptions {
                source: Some(""),
                ..options(version)
            };
            let empty_source = create(dir.path(), empty_source).await.unwrap();
            assert_ne!(plain.info_hash(), private.info_hash());
            assert_ne!(plain.info_hash(), source.info_hash());
            assert_ne!(private.info_hash(), source.info_hash());
            if version.has_v2() {
                assert_ne!(plain.info_hash_v2(), private.info_hash_v2());
                assert_ne!(plain.info_hash_v2(), source.info_hash_v2());
            }
            assert_eq!(hashes(&plain), hashes(&empty_source));
        }
    }

    #[tokio::test]
    async fn test_trackers_and_web_seeds_are_written_like_libtorrent() {
        fn with(trackers: &[&str], web_seeds: &[&str]) -> CreateTorrentOptions<'static> {
            CreateTorrentOptions {
                trackers: trackers.iter().map(|t| t.to_string()).collect(),
                web_seeds: web_seeds.iter().map(|t| t.to_string()).collect(),
                ..Default::default()
            }
        }
        fn bencoded(s: &str) -> String {
            format!("{}:{s}", s.len())
        }

        let dir = folder_with(&files(&[("a", 100)]));
        let a = "http://a.example/announce";
        let b = "udp://b.example:80";
        let w1 = "http://w1.example/files/";
        let w2 = "https://w2.example/x";

        let none = torrent_bytes(dir.path(), with(&[], &[])).await;
        assert!(!contains(&none, b"8:announce"));
        assert!(!contains(&none, b"13:announce-list"));
        assert!(!contains(&none, b"8:url-list"));

        // One tracker: `announce` only. One web seed: a string.
        let one = torrent_bytes(dir.path(), with(&[a], &[w1])).await;
        assert!(contains(
            &one,
            format!("8:announce{}", bencoded(a)).as_bytes()
        ));
        assert!(!contains(&one, b"13:announce-list"));
        assert!(contains(
            &one,
            format!("8:url-list{}", bencoded(w1)).as_bytes()
        ));

        // Two: `announce` is the first, `announce-list` has both; the web seeds are a list.
        let two = torrent_bytes(dir.path(), with(&[a, b], &[w1, w1, w2])).await;
        let (a, b, w1, w2) = (bencoded(a), bencoded(b), bencoded(w1), bencoded(w2));
        assert!(contains(&two, format!("8:announce{a}").as_bytes()));
        assert!(contains(
            &two,
            format!("13:announce-listll{a}{b}ee").as_bytes()
        ));
        assert!(contains(&two, format!("8:url-listl{w1}{w2}e").as_bytes()));
    }

    #[tokio::test]
    async fn test_default_options() {
        let files = files(&[("a", 100_000), ("b/c", 5)]);
        let dir = folder_with(&files);
        let torrent = create(dir.path(), Default::default()).await.unwrap();
        check(&torrent, TorrentVersion::V1, &files);
        let meta = &torrent.meta;
        let info = &meta.info.data;
        assert_eq!(info.piece_length, 16 * 1024);
        assert!(!info.private);
        assert!(info.source.is_none());
        assert!(info.meta_version.is_none());
        assert!(info.file_tree.is_none());
        assert!(meta.piece_layers.is_none());
        assert_eq!(
            text(meta.created_by.as_ref()),
            Some(crate::client_name_and_version())
        );
        assert_eq!(text(meta.encoding.as_ref()), Some("utf-8"));
        assert!(meta.announce.is_none());
        assert!(meta.announce_list.is_empty());
        assert!(meta.url_list.is_empty());
        assert!(meta.comment.is_none());
        assert!(meta.creation_date.is_none());

        // An empty `created by` writes none.
        let anonymous = CreateTorrentOptions {
            created_by: Some(""),
            ..Default::default()
        };
        let anonymous = create(dir.path(), anonymous).await.unwrap();
        assert!(anonymous.meta.created_by.is_none());
        assert_eq!(anonymous.info_hash(), torrent.info_hash());
    }

    #[test]
    fn test_auto_piece_length_table() {
        // The interop tests check that libtorrent picks the same for these sizes.
        for &(total, expected) in AUTO_PIECE_LENGTHS {
            assert_eq!(auto_piece_length(total), expected, "{total}");
        }
        assert_eq!(auto_piece_length(u64::MAX), 16 << 20);
    }

    #[test]
    fn test_explicit_piece_lengths() {
        for valid in [
            MIN_PIECE_LENGTH,
            32 << 10,
            1 << 20,
            16 << 20,
            MAX_PIECE_LENGTH,
        ] {
            assert!(validate_piece_length(valid).is_ok(), "{valid}");
        }
        for invalid in [
            0,
            1,
            1024,
            8 << 10,
            48 << 10,
            3 << 20,
            256 << 20,
            1 << 31,
            u32::MAX,
        ] {
            assert!(validate_piece_length(invalid).is_err(), "{invalid}");
        }
    }

    #[tokio::test]
    async fn test_piece_length_option() {
        let files = files(&[("a", 100), ("b", 3 * PL)]);
        let dir = folder_with(&files);
        for version in FORMATS {
            let invalid = CreateTorrentOptions {
                piece_length: Some(48 << 10),
                ..options(version)
            };
            let error = error_of(dir.path(), invalid).await;
            assert!(error.contains("invalid piece length"), "{error}");

            let large = CreateTorrentOptions {
                piece_length: Some(1 << 20),
                ..options(version)
            };
            let torrent = create(dir.path(), large).await.unwrap();
            assert_eq!(torrent.meta.info.data.piece_length, 1 << 20);
            check(&torrent, version, &files);
        }
    }

    #[test]
    fn test_hybrid_padding_follows_libtorrent() {
        use super::V1Entry::{File as F, Pad};

        let p = u64::from(P);
        let padded = |lengths: &[u64]| v1_entries(lengths.iter().copied(), P, true).unwrap();
        // What libtorrent 2.1 writes for these layouts by default: padding after every
        // non-empty file that does not end on a piece boundary, the last one too, none after
        // empty files, none at all when there is one file.
        assert_eq!(padded(&[1500, 0]), (vec![F(0), Pad(p - 1500), F(1)], p));
        assert_eq!(padded(&[0, 1500]), (vec![F(0), F(1), Pad(p - 1500)], p));
        assert_eq!(
            padded(&[1500, 0, 1500]),
            (vec![F(0), Pad(p - 1500), F(1), F(2), Pad(p - 1500)], 2 * p)
        );
        assert_eq!(
            padded(&[0, 1500, 1500]),
            (vec![F(0), F(1), Pad(p - 1500), F(2), Pad(p - 1500)], 2 * p)
        );
        assert_eq!(padded(&[0, 0, 5]), (vec![F(0), F(1), F(2), Pad(p - 5)], p));
        assert_eq!(padded(&[1500, p]), (vec![F(0), Pad(p - 1500), F(1)], 2 * p));
        assert_eq!(padded(&[p, 1500]), (vec![F(0), F(1), Pad(p - 1500)], 2 * p));
        assert_eq!(padded(&[p, 2 * p]), (vec![F(0), F(1)], 3 * p));
        assert_eq!(
            padded(&[1500, 1500, 0, 0]),
            (
                vec![F(0), Pad(p - 1500), F(1), Pad(p - 1500), F(2), F(3)],
                2 * p
            )
        );
        assert_eq!(padded(&[3 * p + 1]), (vec![F(0)], 3 * p + 1));
        // v1 torrents have none.
        assert_eq!(
            v1_entries([1500, 1500, 0], P, false).unwrap(),
            (vec![F(0), F(1), F(2)], 3000)
        );
    }

    #[tokio::test]
    async fn test_hybrid_padding_files() {
        let dir = folder_with(&files(&[("a", 1500), ("b", 0), ("c", 1500)]));
        let torrent = create(dir.path(), options(TorrentVersion::Hybrid))
            .await
            .unwrap();
        let listed: Vec<(String, u64, Option<&str>)> = torrent
            .meta
            .info
            .data
            .files
            .as_ref()
            .unwrap()
            .iter()
            .map(|f| {
                let path: Vec<&str> = f.path.iter().map(|c| text(Some(c)).unwrap()).collect();
                (path.join("/"), f.length, text(f.attr.as_ref()))
            })
            .collect();
        let pad = u64::from(P) - 1500;
        let pad = (format!(".pad/{pad}"), pad, Some("p"));
        assert_eq!(
            listed,
            [
                ("a".to_owned(), 1500, None),
                pad.clone(),
                ("b".to_owned(), 0, None),
                ("c".to_owned(), 1500, None),
                pad,
            ]
        );
    }

    #[tokio::test]
    async fn test_piece_layers() {
        let same = data(9, 3 * PL + 10);
        let files = vec![
            ("copy1".to_owned(), same.clone()),
            ("copy2".to_owned(), same),
            ("one_piece".to_owned(), data(3, PL)),
            ("small".to_owned(), data(4, 10)),
        ];
        let dir = folder_with(&files);
        for version in [TorrentVersion::V2, TorrentVersion::Hybrid] {
            let torrent = create(dir.path(), options(version)).await.unwrap();
            check(&torrent, version, &files);
            // Only files larger than a piece have a layer, and identical files share it.
            let layers = &torrent.meta.piece_layers.as_ref().unwrap().0;
            assert_eq!(layers.len(), 1);
            assert_eq!(layers[0].1.as_ref().len(), 4 * 32);
        }
    }

    /// Two files of a little more than 9 MiB each.
    fn big_files() -> Vec<TestFile> {
        vec![
            ("a".to_owned(), vec![1; 9 << 20]),
            ("b".to_owned(), vec![2; (9 << 20) + 1]),
        ]
    }

    #[tokio::test]
    async fn test_progress_reaches_total_bytes() {
        let dir = folder_with(&big_files());
        let total = (18 << 20) + 1;
        for version in FORMATS {
            let reports = Arc::new(Mutex::new(Vec::new()));
            let progress: CreateTorrentProgressCallback = {
                let reports = reports.clone();
                Arc::new(move |p: CreateTorrentProgress| reports.lock().unwrap().push(p))
            };
            // Hybrid: the padding is hashed but not counted.
            let options = CreateTorrentOptions {
                progress: Some(progress),
                ..options(version)
            };
            create(dir.path(), options).await.unwrap();
            let reports = reports.lock().unwrap().clone();
            assert!(
                reports.iter().all(|r| r.total_bytes == total),
                "{reports:?}"
            );
            let hashed: Vec<u64> = reports.iter().map(|r| r.hashed_bytes).collect();
            // At the start, every 8 MiB, at the end: not on every read.
            assert_eq!(hashed.len(), 4, "{hashed:?}");
            assert_eq!(hashed.first(), Some(&0));
            assert_eq!(hashed.last(), Some(&total));
            assert!(hashed.windows(2).all(|w| w[1] > w[0]), "{hashed:?}");
            assert!(hashed[1] >= PROGRESS_STEP && hashed[2] - hashed[1] >= PROGRESS_STEP);
        }
    }

    /// The files and folders under `dir` and their lengths.
    fn listing(dir: &Path) -> Vec<(PathBuf, u64)> {
        let mut out: Vec<(PathBuf, u64)> = walkdir::WalkDir::new(dir)
            .into_iter()
            .map(|e| {
                let e = e.unwrap();
                (e.path().to_owned(), e.metadata().unwrap().len())
            })
            .collect();
        out.sort();
        out
    }

    #[tokio::test]
    async fn test_cancel_while_hashing() {
        let dir = folder_with(&big_files());
        let before = listing(dir.path());
        for version in FORMATS {
            let cancel = CancellationToken::new();
            let reports = Arc::new(Mutex::new(Vec::new()));
            let progress: CreateTorrentProgressCallback = {
                let reports = reports.clone();
                let cancel = cancel.clone();
                Arc::new(move |p: CreateTorrentProgress| {
                    reports.lock().unwrap().push(p.hashed_bytes);
                    if p.hashed_bytes > 0 {
                        cancel.cancel();
                    }
                })
            };
            let options = CreateTorrentOptions {
                progress: Some(progress),
                cancel: Some(cancel),
                ..options(version)
            };
            let err = create(dir.path(), options).await.unwrap_err();
            assert!(err.is::<CreateTorrentCancelled>(), "{err:#}");
            // It stopped at the next read: no further report, not even the final one.
            let reports = reports.lock().unwrap().clone();
            assert_eq!(reports.len(), 2, "{reports:?}");
            assert!(reports[1] < (18 << 20) + 1, "{reports:?}");
        }
        // Nothing was written.
        assert_eq!(listing(dir.path()), before);
    }

    #[tokio::test]
    async fn test_cancelled_before_the_start() {
        let dir = folder_with(&files(&[("a", 100)]));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let called = Arc::new(AtomicBool::new(false));
        let progress: CreateTorrentProgressCallback = {
            let called = called.clone();
            Arc::new(move |_| called.store(true, Ordering::SeqCst))
        };
        let options = CreateTorrentOptions {
            progress: Some(progress),
            cancel: Some(cancel),
            ..Default::default()
        };
        let err = create(dir.path(), options).await.unwrap_err();
        assert!(err.is::<CreateTorrentCancelled>(), "{err:#}");
        assert!(!called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_invalid_input() {
        let dir = tempdir();
        let missing = dir.path().join("missing");
        let error = error_of(&missing, Default::default()).await;
        assert!(error.contains("missing"), "{error}");
        let empty = dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        let error = error_of(&empty, Default::default()).await;
        assert!(error.contains("no files"), "{error}");
        let zeros = dir.path().join("zeros");
        write_files(&zeros, &files(&[("a", 0), ("b/c", 0)]));
        let error = error_of(&zeros, Default::default()).await;
        assert!(error.contains("no data"), "{error}");

        let ok = dir.path().join("ok");
        write_files(&ok, &files(&[("a", 10)]));
        fn web_seeds(urls: &[&str]) -> Vec<String> {
            urls.iter().map(|u| u.to_string()).collect()
        }
        let too_many: Vec<String> = (0..=64).map(|i| format!("http://ws{i}.example/")).collect();
        for (expected, options) in [
            (
                "invalid torrent name",
                CreateTorrentOptions {
                    name: Some(""),
                    ..Default::default()
                },
            ),
            (
                "invalid torrent name",
                CreateTorrentOptions {
                    name: Some("a/b"),
                    ..Default::default()
                },
            ),
            (
                "invalid web seed",
                CreateTorrentOptions {
                    web_seeds: web_seeds(&["ftp://example.com/x"]),
                    ..Default::default()
                },
            ),
            (
                "invalid web seed",
                CreateTorrentOptions {
                    web_seeds: web_seeds(&["/relative"]),
                    ..Default::default()
                },
            ),
            (
                "too many web seeds",
                CreateTorrentOptions {
                    web_seeds: too_many,
                    ..Default::default()
                },
            ),
            (
                "invalid creation date",
                CreateTorrentOptions {
                    creation_date: Some(-1),
                    ..Default::default()
                },
            ),
        ] {
            let error = error_of(&ok, options).await;
            assert!(error.contains(expected), "{expected}: {error}");
        }

        // 64 distinct web seeds are fine; duplicates do not count.
        let mut many: Vec<String> = (0..64).map(|i| format!("http://ws{i}.example/")).collect();
        many.push("http://ws0.example/".to_owned());
        let options = CreateTorrentOptions {
            web_seeds: many,
            ..Default::default()
        };
        let torrent = create(&ok, options).await.unwrap();
        assert_eq!(torrent.meta.url_list.len(), 64);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_symbolic_links() {
        use std::os::unix::fs::symlink;

        let real = data(1, 3000);
        let dir = folder_with(&[("real.bin".to_owned(), real.clone())]);
        let outside = folder_with(&files(&[("inner.bin", 10)]));
        // A link to a file is followed, a link to a folder is not.
        symlink(dir.path().join("real.bin"), dir.path().join("link.bin")).unwrap();
        symlink(outside.path(), dir.path().join("folder_link")).unwrap();
        let expected = vec![
            ("link.bin".to_owned(), real.clone()),
            ("real.bin".to_owned(), real),
        ];
        for version in FORMATS {
            let torrent = create(dir.path(), options(version)).await.unwrap();
            check(&torrent, version, &expected);
        }

        // A broken link is an error that names it.
        symlink(dir.path().join("nowhere"), dir.path().join("broken")).unwrap();
        let error = error_of(dir.path(), Default::default()).await;
        assert!(error.contains("broken"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_unreadable_file_is_an_error_naming_it() {
        use std::os::unix::fs::PermissionsExt;

        let dir = folder_with(&files(&[("a", 10), ("secret.bin", 10)]));
        let secret = dir.path().join("secret.bin");
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&secret).is_ok() {
            // Running as root: permissions do not stop reads.
            return;
        }
        let error = error_of(dir.path(), Default::default()).await;
        assert!(error.contains("secret.bin"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_name_that_is_not_utf8_is_an_error() {
        use std::os::unix::ffi::OsStrExt;

        let dir = folder_with(&files(&[("a", 10)]));
        let bad = dir.path().join(std::ffi::OsStr::from_bytes(b"bad\xff.bin"));
        if std::fs::write(&bad, b"x").is_err() {
            // Some file systems (APFS) refuse such names.
            return;
        }
        let error = error_of(dir.path(), Default::default()).await;
        assert!(error.contains("UTF-8"), "{error}");
    }

    async fn piece_count(files: &[(&str, usize)], piece_length: u32) -> usize {
        let dir = tempfile::tempdir().unwrap();
        for (name, len) in files {
            std::fs::write(dir.path().join(name), vec![7u8; *len]).unwrap();
        }
        let torrent = create(
            dir.path(),
            CreateTorrentOptions {
                piece_length: Some(piece_length),
                ..Default::default()
            },
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

    // libtorrent rejects a torrent that has more piece hashes than pieces.
    #[tokio::test]
    async fn test_content_ending_on_a_piece_boundary_has_no_extra_piece() {
        assert_eq!(piece_count(&[("a", 4 * PL)], P).await, 4);
        assert_eq!(piece_count(&[("a", 4 * PL + 1)], P).await, 5);
        assert_eq!(piece_count(&[("a", 1)], P).await, 1);
    }

    #[tokio::test]
    async fn test_empty_file_does_not_drop_the_last_piece() {
        // Whatever order the two files are read in, there are 1.5 pieces of data: two pieces.
        assert_eq!(piece_count(&[("a", PL + PL / 2), ("b", 0)], P).await, 2);
    }
}
