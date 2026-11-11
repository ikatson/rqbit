use bencode::WithRawBytes;
use buffers::{ByteBuf, ByteBufOwned};
use bytes::Bytes;
use clone_to_owned::CloneToOwned;
use encoding_rs::Encoding;
use itertools::Either;
use serde_derive::{Deserialize, Serialize};
use std::{borrow::Cow, collections::HashSet, iter::once, path::PathBuf, sync::Arc};
use tracing::debug;

use crate::{
    Error,
    hash_id::{Id20, Id32},
    lengths::{Lengths, ValidPieceIndex},
    merkle,
    metainfo_v2::{PieceLayers, TorrentVersion, V2FileInfo, V2FileTree, V2Hashes},
};

pub type TorrentMetaV1Borrowed<'a> = TorrentMetaV1<ByteBuf<'a>>;
pub type TorrentMetaV1Owned = TorrentMetaV1<ByteBufOwned>;

pub struct ParsedTorrent<BufType> {
    /// The parsed torrent.
    pub meta: TorrentMetaV1<BufType>,

    /// The raw bytes of the torrent's "info" dict.
    pub info_bytes: BufType,
}

/// Parse torrent metainfo from bytes (includes `info_hash` and, for v2 and
/// hybrid torrents, `info_hash_v2`).
#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
pub fn torrent_from_bytes<'de>(
    buf: &'de [u8],
) -> Result<TorrentMetaV1<ByteBuf<'de>>, bencode::DeserializeError> {
    let mut t: TorrentMetaV1<ByteBuf<'_>> = bencode::from_bytes(buf)
        .inspect_err(|e| tracing::trace!("error deserializing torrent: {e:#}"))
        .map_err(|e| e.into_kind())?;
    let (v1, v2) = compute_info_hashes(&t.info.data, t.info.raw_bytes.as_ref());
    t.info_hash = v1;
    t.info_hash_v2 = v2;
    Ok(t)
}

/// The info hashes of an info dictionary: the `Id20` that identifies the
/// torrent (SHA-1 of the dictionary for v1 and hybrid torrents, the SHA-256
/// truncated to 20 bytes for v2-only torrents) and the full SHA-256 for v2
/// and hybrid torrents.
#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
pub fn compute_info_hashes<BufType>(
    info: &TorrentMetaV1Info<BufType>,
    raw_info: &[u8],
) -> (Id20, Option<Id32>) {
    use sha1w::{ISha1, ISha256};

    let v2 = (info.meta_version == Some(2)).then(|| {
        let mut digest = sha1w::Sha256::new();
        digest.update(raw_info);
        Id32::new(digest.finish())
    });
    let v1 = match (&info.pieces, v2) {
        (None, Some(v2)) => v2.truncate_for_dht(),
        _ => {
            let mut digest = sha1w::Sha1::new();
            digest.update(raw_info);
            Id20::new(digest.finish())
        }
    };
    (v1, v2)
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// A parsed .torrent file.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(bound(
    serialize = "BufType: serde::Serialize + AsRef<[u8]>",
    deserialize = "BufType: serde::Deserialize<'de> + AsRef<[u8]> + Clone"
))]
pub struct TorrentMetaV1<BufType> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub announce: Option<BufType>,
    #[serde(
        rename = "announce-list",
        default = "Vec::new",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub announce_list: Vec<Vec<BufType>>,
    pub info: WithRawBytes<TorrentMetaV1Info<BufType>, BufType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<BufType>,
    #[serde(rename = "created by", skip_serializing_if = "Option::is_none")]
    pub created_by: Option<BufType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<BufType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publisher: Option<BufType>,
    #[serde(rename = "publisher-url", skip_serializing_if = "Option::is_none")]
    pub publisher_url: Option<BufType>,
    #[serde(rename = "creation date", skip_serializing_if = "Option::is_none")]
    pub creation_date: Option<usize>,
    /// BEP 19 web seeds (`url-list`): HTTP(S) URLs serving the torrent's
    /// content. A string or a list of strings in the file; entries that are
    /// not usable `http` / `https` URLs are dropped, duplicates removed and the
    /// count bounded (see [`crate::web_seed`]). BEP 17 `httpseeds` is ignored.
    #[serde(
        rename = "url-list",
        default,
        deserialize_with = "crate::web_seed::deserialize_url_list",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub url_list: Vec<String>,
    /// BEP 52: pieces root -> piece layer of every file larger than a piece.
    #[serde(
        rename = "piece layers",
        default = "none",
        skip_serializing_if = "Option::is_none"
    )]
    pub piece_layers: Option<PieceLayers<BufType>>,

    /// The identifying hash: SHA-1 of the info dictionary for v1 and hybrid
    /// torrents, its SHA-256 truncated to 20 bytes for v2-only torrents.
    #[serde(skip)]
    pub info_hash: Id20,

    /// SHA-256 of the info dictionary for v2 and hybrid torrents.
    #[serde(skip)]
    pub info_hash_v2: Option<Id32>,
}

impl<BufType> TorrentMetaV1<BufType> {
    pub fn iter_announce(&self) -> impl Iterator<Item = &BufType> {
        if self.announce_list.iter().flatten().next().is_some() {
            return itertools::Either::Left(self.announce_list.iter().flatten());
        }
        itertools::Either::Right(self.announce.iter())
    }
}

#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
impl<BufType: AsRef<[u8]> + Clone> TorrentMetaV1<BufType> {
    /// Validate the info dictionary of a .torrent file and load its `piece
    /// layers`, which must hold the layer of every v2 file larger than a
    /// piece. (Metadata fetched for a magnet link has no piece layers: use
    /// [`TorrentMetaV1Info::validate`] there and learn them from peers.)
    pub fn validate_with_piece_layers(&self) -> crate::Result<ValidatedTorrentMetaV1Info<BufType>> {
        let validated = self.info.data.clone().validate()?;
        if validated.version().has_v2() {
            match self.piece_layers.as_ref() {
                Some(layers) => validated.load_piece_layers(layers)?,
                None if !validated.has_all_piece_layers() => {
                    return Err(Error::V2MissingPieceLayers);
                }
                None => {}
            }
            validated.check_all_piece_layers()?;
        }
        Ok(validated)
    }

    /// Like [`Self::validate_with_piece_layers`], but a v2 file whose layer
    /// is missing from `piece layers` (or a missing dictionary) is no error:
    /// its pieces cannot be verified until the layer is learned from peers
    /// (see [`ValidatedTorrentMetaV1Info::can_verify_piece`]). The entries
    /// that are there are still checked against their pieces root.
    pub fn validate_with_known_piece_layers(
        &self,
    ) -> crate::Result<ValidatedTorrentMetaV1Info<BufType>> {
        let validated = self.info.data.clone().validate()?;
        if validated.version().has_v2()
            && let Some(layers) = self.piece_layers.as_ref()
        {
            validated.load_piece_layers(layers)?;
        }
        Ok(validated)
    }
}

/// Main torrent information, shared by .torrent files and magnet link contents.
#[derive(Default, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(bound(
    serialize = "BufType: serde::Serialize + AsRef<[u8]>",
    deserialize = "BufType: serde::Deserialize<'de> + AsRef<[u8]> + Clone"
))]
pub struct TorrentMetaV1Info<BufType> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<BufType>,
    /// `name.utf-8`: the name in UTF-8, set by some clients next to a `name` in a legacy
    /// encoding. Not used for file names on disk; web seed URLs prefer it, like libtorrent.
    #[serde(
        rename = "name.utf-8",
        default = "none",
        skip_serializing_if = "Option::is_none"
    )]
    pub name_utf8: Option<BufType>,
    /// SHA-1 hashes of the pieces. Absent in v2-only torrents.
    #[serde(default = "none", skip_serializing_if = "Option::is_none")]
    pub pieces: Option<BufType>,
    #[serde(rename = "piece length")]
    pub piece_length: u32,

    // Single-file mode
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,
    #[serde(default = "none", skip_serializing_if = "Option::is_none")]
    pub attr: Option<BufType>,
    #[serde(default = "none", skip_serializing_if = "Option::is_none")]
    pub sha1: Option<BufType>,
    #[serde(
        default = "none",
        rename = "symlink path",
        skip_serializing_if = "Option::is_none"
    )]
    pub symlink_path: Option<Vec<BufType>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub md5sum: Option<BufType>,

    // Multi-file mode
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<TorrentMetaV1File<BufType>>>,

    #[serde(skip_serializing_if = "is_false", default)]
    pub private: bool,

    /// BEP 52 `meta version`: 2 for v2 and hybrid torrents.
    #[serde(
        rename = "meta version",
        default = "none",
        skip_serializing_if = "Option::is_none"
    )]
    pub meta_version: Option<u32>,

    /// BEP 52 `file tree` of v2 and hybrid torrents.
    #[serde(
        rename = "file tree",
        default = "none",
        skip_serializing_if = "Option::is_none"
    )]
    pub file_tree: Option<V2FileTree<BufType>>,
}

#[derive(Clone, Copy)]
pub struct FileIteratorName<'a, BufType> {
    encoding: &'static Encoding,
    data: FileIteratorNameData<'a, BufType>,
}

#[derive(Clone, Copy)]
pub enum FileIteratorNameData<'a, BufType> {
    Single(Option<&'a BufType>),
    Tree(&'a [BufType]),
}

impl<BufType> std::fmt::Debug for FileIteratorName<'_, BufType>
where
    BufType: AsRef<[u8]>,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        <Self as std::fmt::Display>::fmt(self, f)
    }
}

impl<BufType> std::fmt::Display for FileIteratorName<'_, BufType>
where
    BufType: AsRef<[u8]>,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (idx, bit) in self.iter_components().enumerate() {
            if idx > 0 {
                write!(f, "{}", std::path::MAIN_SEPARATOR)?;
            }
            write!(f, "{bit}")?;
        }
        Ok(())
    }
}

impl<'a, BufType> FileIteratorName<'a, BufType>
where
    BufType: AsRef<[u8]>,
{
    /// Convert path components into a vector.
    pub fn to_vec(&self) -> Vec<String> {
        self.iter_components().map(|c| c.into_owned()).collect()
    }

    /// Convert path components into a PathBuf, for use with local FS.
    pub fn to_pathbuf(&self) -> PathBuf {
        let mut buf = PathBuf::new();
        for bit in self.iter_components() {
            buf.push(&*bit)
        }
        buf
    }

    /// Iterate path components as strings. Will decode using the detected encoding, replacing
    /// unknown characters with a placeholder.
    pub fn iter_components(&self) -> impl Iterator<Item = Cow<'a, str>> + use<'a, BufType> {
        let encoding = self.encoding;
        self.iter_components_bytes()
            .map(move |part| encoding.decode(part).0)
    }

    /// Iterate path components as bytes.
    pub fn iter_components_bytes(&self) -> impl Iterator<Item = &'a [u8]> + use<'a, BufType> {
        let it = match self.data {
            FileIteratorNameData::Single(None) => {
                return Either::Left(once(&b"torrent-content"[..]));
            }
            FileIteratorNameData::Single(Some(name)) => Either::Left(once((*name).as_ref())),
            FileIteratorNameData::Tree(t) => Either::Right(t.iter().map(|bb| bb.as_ref())),
        };
        Either::Right(it)
    }
}

#[derive(Serialize, Deserialize, Default, Debug, Clone, Copy)]
pub struct FileDetailsAttrs {
    pub symlink: bool,
    pub hidden: bool,
    pub padding: bool,
    pub executable: bool,
}

pub struct FileDetails<'a, BufType> {
    pub filename: FileIteratorName<'a, BufType>,
    pub len: u64,

    // bep-47
    attr: Option<&'a BufType>,
    pub sha1: Option<&'a BufType>,
    pub symlink_path: Option<&'a [BufType]>,
}

impl<BufType> FileDetails<'_, BufType>
where
    BufType: AsRef<[u8]>,
{
    pub fn attrs(&self) -> FileDetailsAttrs {
        let attrs = match self.attr {
            Some(attrs) => attrs,
            None => return FileDetailsAttrs::default(),
        };
        let mut result = FileDetailsAttrs::default();
        for byte in attrs.as_ref().iter().copied() {
            match byte {
                b'l' => result.symlink = true,
                b'h' => result.hidden = true,
                b'p' => result.padding = true,
                b'x' => result.executable = true,
                other => debug!(attr = other, "unknown file attribute"),
            }
        }
        result
    }
}

pub struct FileDetailsExt<'a, BufType> {
    pub details: FileDetails<'a, BufType>,
    // absolute offset in torrent if it was a flat blob of bytes
    pub offset: u64,

    // the pieces that contain this file
    pub pieces: std::ops::Range<u32>,
}

impl<BufType> FileDetailsExt<'_, BufType> {
    pub fn pieces_usize(&self) -> std::ops::Range<usize> {
        self.pieces.start as usize..self.pieces.end as usize
    }
}

#[derive(Clone, Debug)]
pub struct ValidatedTorrentMetaV1Info<BufType> {
    encoding: &'static Encoding,
    lengths: Lengths,
    info: TorrentMetaV1Info<BufType>,
    version: TorrentVersion,
    // v2 and hybrid torrents. Shared between clones, so piece layers learned
    // later are seen by every holder.
    v2: Option<Arc<V2Hashes>>,
}

impl<BufType: AsRef<[u8]>> ValidatedTorrentMetaV1Info<BufType> {
    pub fn name(&self) -> Option<Cow<'_, str>> {
        self.info
            .name
            .as_ref()
            .map(|n| self.encoding.decode(n.as_ref()).0)
            .filter(|n| !n.is_empty())
    }

    pub fn info(&self) -> &TorrentMetaV1Info<BufType> {
        &self.info
    }

    pub fn lengths(&self) -> &Lengths {
        &self.lengths
    }

    /// The version of the torrent.
    pub fn version(&self) -> TorrentVersion {
        self.version
    }

    pub fn name_or_else<'a, DefaultT: Into<Cow<'a, str>>>(
        &'a self,
        default: impl Fn() -> DefaultT,
    ) -> Cow<'a, str> {
        self.name().unwrap_or_else(|| default().into())
    }

    /// Guaranteed to produce at least one file.
    pub fn iter_file_details(&self) -> impl Iterator<Item = FileDetails<'_, BufType>> {
        // .unwrap is ok here() as we checked errors at creation time.
        self.info.iter_file_details_raw(self.encoding).unwrap()
    }

    pub fn iter_file_lengths(&self) -> impl Iterator<Item = u64> + '_ {
        self.iter_file_details().map(|d| d.len)
    }

    // Iterate file details with additional computations for offsets.
    pub fn iter_file_details_ext<'a>(
        &'a self,
    ) -> impl Iterator<Item = FileDetailsExt<'a, BufType>> + 'a {
        // v2-only torrents start every file on a piece boundary (empty files
        // too, as libtorrent places them, though they cover no piece).
        let align = match self.version {
            TorrentVersion::V2 => self.lengths.default_piece_length() as u64,
            TorrentVersion::V1 | TorrentVersion::Hybrid => 1,
        };
        self.iter_file_details()
            .scan(0u64, move |acc_offset, details| {
                let offset = acc_offset.next_multiple_of(align);
                *acc_offset = offset + details.len;
                Some(FileDetailsExt {
                    pieces: self.lengths.iter_pieces_within_offset(offset, details.len),
                    details,
                    offset,
                })
            })
    }

    /// The non-empty files of a v2 or hybrid torrent, in piece order. Empty
    /// for v1 torrents.
    pub fn v2_files(&self) -> &[V2FileInfo] {
        self.v2.as_deref().map(|v| &*v.files).unwrap_or(&[])
    }

    /// The position in [`Self::v2_files`] of the file with this pieces root.
    pub fn v2_file_by_root(&self, pieces_root: &Id32) -> Option<usize> {
        let v2 = self.v2.as_deref()?;
        let pos = v2
            .by_root
            .binary_search_by(|(r, _)| r.cmp(pieces_root))
            .ok()?;
        v2.by_root.get(pos).map(|(_, idx)| *idx as usize)
    }

    /// The file of a v2 or hybrid torrent that `piece` belongs to, with its
    /// position in [`Self::v2_files`].
    pub fn v2_file_for_piece(&self, piece: ValidPieceIndex) -> Option<(usize, &V2FileInfo)> {
        let v2 = self.v2.as_deref()?;
        let pos = v2.file_for_piece(piece.get())?;
        v2.files.get(pos).map(|f| (pos, f))
    }

    /// The piece layer of the file at `file` in [`Self::v2_files`], if known.
    /// Always None for files that are not larger than a piece.
    pub fn piece_layer(&self, file: usize) -> Option<&[Id32]> {
        self.v2.as_deref()?.layers.get(file)?.get().map(|l| &**l)
    }

    /// Positions in [`Self::v2_files`] of the files whose piece layer is
    /// needed but not known yet.
    pub fn missing_piece_layers(&self) -> impl Iterator<Item = usize> + '_ {
        let pl = self.lengths.default_piece_length();
        self.v2_files()
            .iter()
            .enumerate()
            .filter(move |(idx, f)| f.has_piece_layer(pl) && self.piece_layer(*idx).is_none())
            .map(|(idx, _)| idx)
    }

    /// True if every piece can be verified (always for v1 torrents).
    pub fn has_all_piece_layers(&self) -> bool {
        self.missing_piece_layers().next().is_none()
    }

    /// Error naming the first file whose piece layer is missing, if any. A
    /// .torrent file must carry all of them.
    pub fn check_all_piece_layers(&self) -> crate::Result<()> {
        let Some(missing) = self.missing_piece_layers().next() else {
            return Ok(());
        };
        let name = self
            .v2_files()
            .get(missing)
            .and_then(|f| self.iter_file_details().nth(f.file_index))
            .map(|d| d.filename.to_string())
            .unwrap_or_default();
        Err(Error::V2MissingPieceLayersEntry(name))
    }

    /// True if the piece's expected hash is known (v1 torrents, files up to
    /// one piece, files whose piece layer is known).
    pub fn can_verify_piece(&self, piece: ValidPieceIndex) -> bool {
        match self.v2_file_for_piece(piece) {
            Some((pos, f)) => {
                !f.has_piece_layer(self.lengths.default_piece_length())
                    || self.piece_layer(pos).is_some()
            }
            // v1 torrents, and pieces of hybrid torrents that hold nothing
            // but padding: SHA-1 alone.
            None => self.version.has_v1(),
        }
    }

    /// Set the piece layer of the file at `file` in [`Self::v2_files`] (and of
    /// any other file with the same pieces root) after checking that it hashes
    /// to the file's pieces root. Returns false if it was already known.
    #[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
    pub fn set_piece_layer(&self, file: usize, layer: &[Id32]) -> crate::Result<bool> {
        let pl = self.lengths.default_piece_length();
        let v2 = self.v2.as_deref().ok_or(Error::V2MissingFileTree)?;
        let f = v2
            .files
            .get(file)
            .ok_or_else(|| Error::V2MissingPieceLayersEntry(format!("file #{file}")))?;
        if !f.has_piece_layer(pl) {
            return Err(Error::V2SmallFileShouldNotHavePieceLayers);
        }
        if layer.len() != f.num_pieces as usize {
            return Err(Error::V2PieceLayersWrongSize {
                expected: f.num_pieces as usize * 32,
                actual: layer.len().saturating_mul(32),
            });
        }
        if merkle::pieces_root_from_piece_layer(layer, pl) != Some(f.pieces_root) {
            return Err(Error::V2PieceLayersRootMismatch);
        }
        let mut set = false;
        for (other, slot) in v2.files.iter().zip(v2.layers.iter()) {
            if other.pieces_root == f.pieces_root {
                set |= slot.set(layer.into()).is_ok();
            }
        }
        Ok(set)
    }

    /// Load the entries of a `piece layers` dictionary, checking each one.
    /// Entries for unknown roots, or for files that need no layer, are
    /// ignored like libtorrent does. Missing entries are not an error here,
    /// see [`Self::check_all_piece_layers`].
    #[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
    pub fn load_piece_layers<L: AsRef<[u8]>>(&self, layers: &PieceLayers<L>) -> crate::Result<()> {
        let pl = self.lengths.default_piece_length();
        for (root, hashes) in layers.0.iter() {
            let root = root.as_ref();
            let root =
                Id32::from_bytes(root).map_err(|_| Error::V2InvalidPiecesRoot(root.len()))?;
            let Some((file, info)) = self
                .v2_file_by_root(&root)
                .and_then(|idx| Some((idx, self.v2_files().get(idx)?)))
            else {
                debug!(?root, "ignoring piece layer of unknown file");
                continue;
            };
            if !info.has_piece_layer(pl) {
                debug!(?root, "ignoring piece layer of a file of at most one piece");
                continue;
            }
            let hashes = hashes.as_ref();
            let (chunks, rest) = hashes.as_chunks::<32>();
            if !rest.is_empty() {
                return Err(Error::V2PieceLayersWrongSize {
                    expected: info.num_pieces as usize * 32,
                    actual: hashes.len(),
                });
            }
            let layer: Vec<Id32> = chunks.iter().map(|c| Id32::new(*c)).collect();
            self.set_piece_layer(file, &layer)?;
        }
        Ok(())
    }

    /// The piece layers known so far, as the `piece layers` dictionary of a
    /// torrent file.
    pub fn known_piece_layers(&self) -> PieceLayers<ByteBufOwned> {
        let mut out: Vec<(ByteBufOwned, ByteBufOwned)> = Vec::new();
        let Some(v2) = self.v2.as_deref() else {
            return PieceLayers(out);
        };
        for (root, idx) in v2.by_root.iter() {
            if out.last().is_some_and(|(k, _)| k.as_ref() == root.0) {
                continue;
            }
            let Some(layer) = self.piece_layer(*idx as usize) else {
                continue;
            };
            let mut v = Vec::with_capacity(layer.len() * 32);
            for h in layer {
                v.extend_from_slice(&h.0);
            }
            out.push((
                ByteBufOwned(Bytes::copy_from_slice(&root.0)),
                ByteBufOwned(Bytes::from(v)),
            ));
        }
        PieceLayers(out)
    }

    /// A verifier for one piece: SHA-1 for v1, the merkle hash for v2, both
    /// for hybrid torrents. None if the expected hash is not known (v2 file
    /// whose piece layer is missing, v1 torrent without the piece's hash).
    #[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
    pub fn piece_verifier(
        &self,
        piece: ValidPieceIndex,
    ) -> Option<crate::metainfo_v2::PieceVerifier> {
        let len = self.lengths.piece_length(piece);
        let mut verifier = crate::metainfo_v2::PieceVerifier::new(len);
        if self.version.has_v1() {
            let expected: [u8; 20] = self.info.get_hash(piece.get())?.try_into().ok()?;
            verifier = verifier.with_v1(expected);
        }
        if self.version.has_v2()
            && let Some((pos, f)) = self.v2_file_for_piece(piece)
        {
            let pl = self.lengths.default_piece_length();
            let in_file = u64::from(piece.get() - f.first_piece);
            let file_bytes = f
                .length
                .checked_sub(in_file * u64::from(pl))?
                .min(u64::from(pl));
            let (expected, leaves) = if f.has_piece_layer(pl) {
                let layer = self.piece_layer(pos)?;
                (
                    *layer.get(usize::try_from(in_file).ok()?)?,
                    u64::from(pl / merkle::BLOCK_SIZE),
                )
            } else {
                (f.pieces_root, merkle::num_leafs(f.length)?)
            };
            verifier = verifier.with_v2(expected, leaves, file_bytes);
        } else if !self.version.has_v1() {
            // A v2 piece outside of every file does not exist.
            return None;
        }
        Some(verifier)
    }
}

impl<BufType: AsRef<[u8]>> TorrentMetaV1Info<BufType> {
    /// The version of the torrent: v1 without `meta version`, v2 or hybrid
    /// (depending on `pieces`) with `meta version` 2.
    pub fn version(&self) -> crate::Result<TorrentVersion> {
        match self.meta_version {
            None if self.pieces.is_some() => Ok(TorrentVersion::V1),
            None => Err(Error::BadTorrentMissingPieces),
            Some(2) if self.file_tree.is_none() => Err(Error::V2MissingFileTree),
            Some(2) if self.pieces.is_some() => Ok(TorrentVersion::Hybrid),
            Some(2) => Ok(TorrentVersion::V2),
            Some(v) => Err(Error::V2UnsupportedMetaVersion(v)),
        }
    }

    fn validate_file_tree(&self) -> crate::Result<()> {
        use crate::metainfo_v2::{
            FileTreeProblem, MAX_FILE_TREE_DEPTH, MAX_FILE_TREE_FILES,
            MAX_FILE_TREE_PATH_COMPONENTS,
        };

        if merkle::piece_layer_height(self.piece_length).is_none() {
            return Err(Error::V2InvalidPieceLength(self.piece_length));
        }
        let tree = self.file_tree.as_ref().ok_or(Error::V2MissingFileTree)?;
        match tree.problem {
            None => {}
            Some(FileTreeProblem::TooDeep) => {
                return Err(Error::V2FileTreeTooDeep(MAX_FILE_TREE_DEPTH));
            }
            Some(FileTreeProblem::TooManyFiles) => {
                return Err(Error::V2FileTreeTooManyFiles(MAX_FILE_TREE_FILES));
            }
            Some(FileTreeProblem::TooManyPathComponents) => {
                return Err(Error::V2FileTreeTooManyPathComponents(
                    MAX_FILE_TREE_PATH_COMPONENTS,
                ));
            }
            Some(FileTreeProblem::MixedNode) => return Err(Error::V2FileTreeMixedNode),
            Some(FileTreeProblem::RootIsFile) => return Err(Error::V2FileTreeRootIsFile),
        }
        if tree.files.is_empty() {
            return Err(Error::BadTorrentNoFiles);
        }
        // Files that share a pieces root share hashes. That is fine for
        // identical files, but two files of different lengths cannot have the
        // same content (a short file can be crafted to hash to an inner node of
        // a longer one's tree); libtorrent rejects these too.
        let mut roots = std::collections::HashMap::<Id32, u64>::new();
        for f in tree.files.iter() {
            if f.path.iter().any(|c| c.as_ref() == b".") {
                return Err(Error::V2FileTreeDotComponent);
            }
            match (f.length, f.pieces_root.as_ref()) {
                (0, None) => {}
                (0, Some(_)) => return Err(Error::V2ZeroLengthFileHasPiecesRoot),
                (_, None) => return Err(Error::V2SmallFileMissingPiecesRoot),
                (len, Some(root)) => {
                    let root = Id32::from_bytes(root.as_ref())
                        .map_err(|_| Error::V2InvalidPiecesRoot(root.as_ref().len()))?;
                    if *roots.entry(root).or_insert(len) != len {
                        return Err(Error::V2DuplicatePiecesRoot);
                    }
                }
            }
        }
        Ok(())
    }

    pub fn validate(self) -> crate::Result<ValidatedTorrentMetaV1Info<BufType>> {
        let version = self.version()?;
        if version.has_v2() {
            self.validate_file_tree()?;
        }
        let lengths = Lengths::from_torrent(&self)?;
        let encoding = self.detect_encoding();
        let mut validated = ValidatedTorrentMetaV1Info {
            encoding,
            lengths,
            info: self,
            version,
            v2: None,
        };

        // Ensure:
        // - there's at least one file
        // - each filename has at least one path component
        // - there's no path traversal
        // - all non-padding filenames are unique
        let mut seen_files = 0;
        for file in validated.info.iter_file_details_raw(encoding)? {
            seen_files += 1;
            let mut seen_a_bit = false;
            for bit in file.filename.iter_components_bytes() {
                seen_a_bit = true;
                if bit == b".." {
                    return Err(Error::BadTorrentPathTraversal);
                }
                use memchr::memchr;
                if memchr(b'/', bit).is_some() || memchr(b'\\', bit).is_some() {
                    return Err(Error::BadTorrentSeparatorInName);
                }
            }
            if !seen_a_bit {
                return Err(Error::BadTorrentFileNoName);
            }
        }
        if seen_files == 0 {
            return Err(Error::BadTorrentNoFiles);
        }

        // Padding files (BEP 47 "p" attr; the BEP 52 v1-compat layer names
        // them ".pad/<length>") are never materialized on disk - every
        // storage backend skips them (filesystem storage doesn't create
        // them, file_ops reads/writes ignore them) - and their conventional
        // name is just the pad length, so two equal-length pads legitimately
        // share a path. Exempt padding from the uniqueness check so hybrid
        // v1+v2 torrents with repeated pad lengths validate; duplicate
        // non-padding filenames still error (they would genuinely share one
        // on-disk file).
        let mut unique_filenames = HashSet::<PathBuf>::new();
        let mut unique_checkable = 0usize;
        for fd in validated.iter_file_details() {
            if fd.attrs().padding {
                continue;
            }
            unique_checkable += 1;
            let pb = fd.filename.to_pathbuf();
            if pb.as_os_str().is_empty() {
                return Err(Error::BadTorrentFileNoName);
            }
            unique_filenames.insert(pb);
        }
        if unique_filenames.len() != unique_checkable {
            return Err(Error::BadTorrentDuplicateFilenames);
        }

        if version.has_v2() {
            validated.v2 = Some(V2Hashes::new(validated.build_v2_files()?));
        }

        Ok(validated)
    }

    /// The SHA-1 hash of a piece (v1 and hybrid torrents).
    pub fn get_hash(&self, piece: u32) -> Option<&[u8]> {
        let start = (piece as usize).checked_mul(20)?;
        let end = start.checked_add(20)?;
        let expected_hash = self.pieces.as_ref()?.as_ref().get(start..end)?;
        Some(expected_hash)
    }

    /// Compare the SHA-1 hash of a piece (v1 and hybrid torrents). None if
    /// the torrent has no SHA-1 hash for the piece.
    pub fn compare_hash(&self, piece: u32, hash: [u8; 20]) -> Option<bool> {
        Some(self.get_hash(piece)? == hash)
    }

    pub fn detect_encoding(&self) -> &'static Encoding {
        let mut encdetect = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
        if let Some(name) = self.name.as_ref() {
            encdetect.feed(name.as_ref(), false);
        }

        for file in self.files.iter().flat_map(|f| f.iter()) {
            for component in file.path.iter() {
                encdetect.feed(component.as_ref(), false);
            }
        }
        if self.files.is_none() && self.length.is_none() {
            for file in self.file_tree.iter().flat_map(|t| t.files.iter()) {
                for component in file.path.iter() {
                    encdetect.feed(component.as_ref(), false);
                }
            }
        }

        encdetect.guess(None, chardetng::Utf8Detection::Allow)
    }

    pub(crate) fn iter_file_details_raw(
        &self,
        encoding: &'static Encoding,
    ) -> crate::Result<impl Iterator<Item = FileDetails<'_, BufType>>> {
        // v2-only torrents describe their files only in the file tree. Hybrid
        // torrents use the v1 list, which has the padding files.
        if self.pieces.is_none() && self.meta_version == Some(2) {
            let tree = self.file_tree.as_ref().ok_or(Error::V2MissingFileTree)?;
            if tree.files.is_empty() {
                return Err(Error::BadTorrentNoFiles);
            }
            return Ok(Either::Right(tree.files.iter().map(move |f| FileDetails {
                filename: FileIteratorName {
                    encoding,
                    data: FileIteratorNameData::Tree(&f.path),
                },
                len: f.length,
                attr: f.attr.as_ref(),
                sha1: None,
                symlink_path: None,
            })));
        }
        let it = match (self.length, self.files.as_ref()) {
            // Single-file
            (Some(length), None) => Either::Left(once(FileDetails {
                filename: FileIteratorName {
                    encoding,
                    data: FileIteratorNameData::Single(self.name.as_ref()),
                },
                len: length,
                attr: self.attr.as_ref(),
                sha1: self.sha1.as_ref(),
                symlink_path: self.symlink_path.as_deref(),
            })),

            // Multi-file
            (None, Some(files)) => {
                if files.is_empty() {
                    return Err(Error::BadTorrentMultiFileEmpty);
                }
                Either::Right(files.iter().map(move |f| FileDetails {
                    filename: FileIteratorName {
                        encoding,
                        data: FileIteratorNameData::Tree(&f.path),
                    },
                    len: f.length,
                    attr: f.attr.as_ref(),
                    sha1: f.sha1.as_ref(),
                    symlink_path: f.symlink_path.as_deref(),
                }))
            }
            _ => return Err(Error::BadTorrentBothSingleAndMultiFile),
        };
        Ok(Either::Left(it))
    }
}

impl<BufType: AsRef<[u8]>> ValidatedTorrentMetaV1Info<BufType> {
    /// Place the non-empty files of a v2 or hybrid torrent in piece space,
    /// checking for hybrid torrents that both file lists describe the same
    /// files and that every file starts on a piece boundary.
    fn build_v2_files(&self) -> crate::Result<Vec<V2FileInfo>> {
        let tree = self
            .info
            .file_tree
            .as_ref()
            .ok_or(Error::V2MissingFileTree)?;
        let pl = u64::from(self.lengths.default_piece_length());
        let num_pieces =
            |len: u64| u32::try_from(len.div_ceil(pl)).map_err(|_| Error::BadTorrentTooManyPieces);
        let root = |f: &crate::metainfo_v2::V2FileTreeEntry<BufType>| {
            f.pieces_root_id()
                .ok_or(Error::V2SmallFileMissingPiecesRoot)
        };
        let mut out = Vec::new();
        match self.version {
            TorrentVersion::V1 => {}
            TorrentVersion::V2 => {
                let mut next_piece = 0u32;
                for (file_index, f) in tree.files.iter().enumerate() {
                    if f.length == 0 {
                        continue;
                    }
                    let n = num_pieces(f.length)?;
                    out.push(V2FileInfo {
                        file_index,
                        length: f.length,
                        pieces_root: root(f)?,
                        first_piece: next_piece,
                        num_pieces: n,
                    });
                    next_piece = next_piece
                        .checked_add(n)
                        .ok_or(Error::BadTorrentTooManyPieces)?;
                }
            }
            TorrentVersion::Hybrid => {
                let mut v2_files = tree.files.iter().filter(|f| f.length > 0);
                // Like libtorrent, only non-empty files must match. They must
                // also sit exactly where the v2 layout puts them, so that both
                // swarms number the pieces the same way.
                let mut next_piece = 0u32;
                for (file_index, fd) in self.iter_file_details_ext().enumerate() {
                    if fd.details.len == 0 || fd.details.attrs().padding {
                        continue;
                    }
                    let name = || fd.details.filename.to_string();
                    let Some(v2) = v2_files.next() else {
                        return Err(Error::V2HybridFileListMismatch(name()));
                    };
                    let same_path = fd
                        .details
                        .filename
                        .iter_components_bytes()
                        .eq(v2.path.iter().map(|c| c.as_ref()));
                    if !same_path || v2.length != fd.details.len {
                        return Err(Error::V2HybridFileListMismatch(name()));
                    }
                    if fd.offset != u64::from(next_piece) * pl {
                        return Err(Error::V2HybridFileNotAligned(name()));
                    }
                    let n = num_pieces(v2.length)?;
                    out.push(V2FileInfo {
                        file_index,
                        length: v2.length,
                        pieces_root: root(v2)?,
                        first_piece: next_piece,
                        num_pieces: n,
                    });
                    next_piece = next_piece
                        .checked_add(n)
                        .ok_or(Error::BadTorrentTooManyPieces)?;
                }
                if let Some(extra) = v2_files.next() {
                    let name = extra
                        .path
                        .iter()
                        .map(|c| String::from_utf8_lossy(c.as_ref()))
                        .collect::<Vec<_>>()
                        .join("/");
                    return Err(Error::V2HybridFileListMismatch(name));
                }
            }
        }
        Ok(out)
    }
}

const fn none<T>() -> Option<T> {
    None
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct TorrentMetaV1File<BufType> {
    pub length: u64,
    pub path: Vec<BufType>,
    /// `path.utf-8`: the path in UTF-8, set by some clients next to a `path` in a legacy
    /// encoding. Not used for file names on disk; web seed URLs prefer it, like libtorrent.
    #[serde(
        rename = "path.utf-8",
        default = "none",
        skip_serializing_if = "Option::is_none"
    )]
    pub path_utf8: Option<Vec<BufType>>,

    #[serde(default = "none", skip_serializing_if = "Option::is_none")]
    pub attr: Option<BufType>,
    #[serde(default = "none", skip_serializing_if = "Option::is_none")]
    pub sha1: Option<BufType>,
    #[serde(
        default = "none",
        rename = "symlink path",
        skip_serializing_if = "Option::is_none"
    )]
    pub symlink_path: Option<Vec<BufType>>,
}

impl<BufType> CloneToOwned for TorrentMetaV1File<BufType>
where
    BufType: CloneToOwned,
{
    type Target = TorrentMetaV1File<<BufType as CloneToOwned>::Target>;

    fn clone_to_owned(&self, within_buffer: Option<&Bytes>) -> Self::Target {
        TorrentMetaV1File {
            length: self.length,
            path: self.path.clone_to_owned(within_buffer),
            path_utf8: self.path_utf8.clone_to_owned(within_buffer),
            attr: self.attr.clone_to_owned(within_buffer),
            sha1: self.sha1.clone_to_owned(within_buffer),
            symlink_path: self.symlink_path.clone_to_owned(within_buffer),
        }
    }
}

impl<BufType> CloneToOwned for TorrentMetaV1Info<BufType>
where
    BufType: CloneToOwned,
{
    type Target = TorrentMetaV1Info<<BufType as CloneToOwned>::Target>;

    fn clone_to_owned(&self, within_buffer: Option<&Bytes>) -> Self::Target {
        TorrentMetaV1Info {
            name: self.name.clone_to_owned(within_buffer),
            name_utf8: self.name_utf8.clone_to_owned(within_buffer),
            pieces: self.pieces.clone_to_owned(within_buffer),
            piece_length: self.piece_length,
            length: self.length,
            md5sum: self.md5sum.clone_to_owned(within_buffer),
            files: self.files.clone_to_owned(within_buffer),
            attr: self.attr.clone_to_owned(within_buffer),
            sha1: self.sha1.clone_to_owned(within_buffer),
            symlink_path: self.symlink_path.clone_to_owned(within_buffer),
            private: self.private,
            meta_version: self.meta_version,
            file_tree: self.file_tree.clone_to_owned(within_buffer),
        }
    }
}

impl<BufType> CloneToOwned for TorrentMetaV1<BufType>
where
    BufType: CloneToOwned,
{
    type Target = TorrentMetaV1<<BufType as CloneToOwned>::Target>;

    fn clone_to_owned(&self, within_buffer: Option<&Bytes>) -> Self::Target {
        TorrentMetaV1 {
            announce: self.announce.clone_to_owned(within_buffer),
            announce_list: self.announce_list.clone_to_owned(within_buffer),
            info: self.info.clone_to_owned(within_buffer),
            comment: self.comment.clone_to_owned(within_buffer),
            created_by: self.created_by.clone_to_owned(within_buffer),
            encoding: self.encoding.clone_to_owned(within_buffer),
            publisher: self.publisher.clone_to_owned(within_buffer),
            publisher_url: self.publisher_url.clone_to_owned(within_buffer),
            creation_date: self.creation_date,
            url_list: self.url_list.clone(),
            piece_layers: self.piece_layers.clone_to_owned(within_buffer),
            info_hash: self.info_hash,
            info_hash_v2: self.info_hash_v2,
        }
    }
}

#[cfg(test)]
mod tests {
    use bencode::{BencodeValue, from_bytes};

    use super::*;

    const TORRENT_BYTES: &[u8] =
        include_bytes!("../../librqbit/resources/ubuntu-21.04-desktop-amd64.iso.torrent");

    #[test]
    fn test_deserialize_torrent_borrowed() {
        let torrent: TorrentMetaV1Borrowed = from_bytes(TORRENT_BYTES).unwrap();
        dbg!(torrent);
    }

    #[test]
    #[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
    fn test_deserialize_torrent_with_info_hash() {
        let torrent: TorrentMetaV1Borrowed = torrent_from_bytes(TORRENT_BYTES).unwrap();
        assert_eq!(
            torrent.info_hash.as_string(),
            "64a980abe6e448226bb930ba061592e44c3781a1"
        );
    }

    #[test]
    fn test_serialize_then_deserialize_bencode() {
        let torrent = from_bytes::<TorrentMetaV1<ByteBuf>>(TORRENT_BYTES)
            .unwrap()
            .info
            .data;
        let mut writer = Vec::new();
        bencode::bencode_serialize_to_writer(&torrent, &mut writer).unwrap();
        let deserialized = from_bytes::<TorrentMetaV1Info<ByteBuf>>(&writer).unwrap();

        assert_eq!(torrent, deserialized);
    }

    #[test]
    fn test_private_serialize_deserialize() {
        for private in [false, true] {
            let info: TorrentMetaV1Info<ByteBufOwned> = TorrentMetaV1Info {
                private,
                ..Default::default()
            };
            let mut buf = Vec::new();
            bencode::bencode_serialize_to_writer(&info, &mut buf).unwrap();

            let deserialized = from_bytes::<TorrentMetaV1Info<ByteBuf>>(&buf).unwrap();
            assert_eq!(info.private, deserialized.private);

            let deserialized_dyn = ::bencode::dyn_from_bytes::<ByteBuf>(&buf).unwrap();
            let hm = match deserialized_dyn {
                bencode::BencodeValue::Dict(hm) => hm,
                _ => panic!("expected dict"),
            };
            match (private, hm.get(&ByteBuf(b"private"))) {
                (true, Some(BencodeValue::Integer(1))) => {}
                (false, None) => {}
                (_, v) => {
                    panic!("unexpected value for \"private\": {v:?}")
                }
            }
        }
    }

    #[test]
    #[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
    fn test_private_real_torrent() {
        let buf = include_bytes!("resources/test/private.torrent");
        let torrent: TorrentMetaV1Borrowed = from_bytes(buf).unwrap();
        assert!(torrent.info.data.private);
    }

    // BEP 47 padding files ("p" attr) are synthetic and never written to
    // disk, and their recommended path is [".pad", "<length>"], so two
    // equal-length pads legitimately share a name. Hybrid v1+v2 (BEP 52)
    // torrents rely on this: the v1 compat layer repeats ".pad/<length>"
    // whenever two files need the same alignment padding.
    #[test]
    fn test_validate_duplicate_pad_paths() {
        let bytes = harness_padded_multi(
            "hybrid",
            &[("a.bin", 1000), ("b.bin", 1000)],
            &[("42", 42), ("42", 42)],
        );
        let torrent = from_bytes::<TorrentMetaV1Borrowed>(&bytes).unwrap();
        let validated = torrent.info.data.validate().unwrap();

        // Pads still count as file entries, but only the real files make it
        // to disk / into "unique" accounting.
        let details: Vec<_> = validated.iter_file_details().collect();
        assert_eq!(details.len(), 4);
        assert_eq!(details.iter().filter(|d| !d.attrs().padding).count(), 2);
        assert_eq!(validated.lengths().total_length(), 2084);
    }

    #[test]
    fn test_validate_duplicate_real_filenames() {
        let bytes = harness_multi_named("dup", &[("same.bin", 10), ("same.bin", 10)]);
        let torrent = from_bytes::<TorrentMetaV1Borrowed>(&bytes).unwrap();
        let err = torrent.info.data.validate().unwrap_err();
        assert!(
            matches!(err, Error::BadTorrentDuplicateFilenames),
            "duplicate real filenames must still be rejected, got {err:?}"
        );
    }

    // A single-file torrent with `url-list` set to the given raw bencoded value.
    fn torrent_with_url_list(url_list: Option<&[u8]>) -> Vec<u8> {
        let info = bdict(&[
            (b"length".as_slice(), bint(10)),
            (b"name".as_slice(), bstr(b"f")),
            (b"piece length".as_slice(), bint(16384)),
            (b"pieces".as_slice(), bstr(&[0u8; 20])),
        ]);
        let mut entries: Vec<(&[u8], Vec<u8>)> = vec![(b"info".as_slice(), info)];
        if let Some(v) = url_list {
            entries.push((b"url-list".as_slice(), v.to_vec()));
        }
        bdict(&entries)
    }

    fn blist(items: &[Vec<u8>]) -> Vec<u8> {
        let mut o = vec![b'l'];
        for i in items {
            o.extend_from_slice(i);
        }
        o.push(b'e');
        o
    }

    #[test]
    fn test_url_list_absent() {
        let bytes = torrent_with_url_list(None);
        let t = from_bytes::<TorrentMetaV1Borrowed>(&bytes).unwrap();
        assert!(t.url_list.is_empty());
    }

    #[test]
    fn test_url_list_string() {
        let bytes = torrent_with_url_list(Some(&bstr(b"http://example.com/f")));
        let t = from_bytes::<TorrentMetaV1Borrowed>(&bytes).unwrap();
        assert_eq!(t.url_list, vec!["http://example.com/f"]);

        // A string that is not a usable URL is dropped.
        let bytes = torrent_with_url_list(Some(&bstr(b"ftp://example.com/f")));
        let t = from_bytes::<TorrentMetaV1Borrowed>(&bytes).unwrap();
        assert!(t.url_list.is_empty());
    }

    #[test]
    fn test_url_list_list_skips_garbage_and_duplicates() {
        let value = blist(&[
            bstr(b"http://a.example/f"),
            bint(5),
            bstr(b"ftp://x/"),
            blist(&[bstr(b"http://nested.example/")]),
            bdict(&[(b"k".as_slice(), bstr(b"http://dict.example/"))]),
            bstr(b"http://a.example/f"),
            bstr(b"\xff\xfe"),
            bstr(b""),
            bstr(b"https://b.example/dir/"),
        ]);
        let bytes = torrent_with_url_list(Some(&value));
        let t = from_bytes::<TorrentMetaV1Borrowed>(&bytes).unwrap();
        assert_eq!(
            t.url_list,
            vec!["http://a.example/f", "https://b.example/dir/"]
        );
    }

    #[test]
    fn test_url_list_bounded() {
        let items: Vec<Vec<u8>> = (0..crate::web_seed::MAX_WEB_SEEDS + 10)
            .map(|i| bstr(format!("http://h{i}.example/").as_bytes()))
            .collect();
        let bytes = torrent_with_url_list(Some(&blist(&items)));
        let t = from_bytes::<TorrentMetaV1Borrowed>(&bytes).unwrap();
        assert_eq!(t.url_list.len(), crate::web_seed::MAX_WEB_SEEDS);
        assert_eq!(t.url_list[0], "http://h0.example/");
    }

    #[test]
    fn test_url_list_of_wrong_type_is_ignored() {
        for value in [
            bint(7),
            bdict(&[(b"a".as_slice(), bstr(b"http://x/"))]),
            blist(&[]),
        ] {
            let bytes = torrent_with_url_list(Some(&value));
            let t = from_bytes::<TorrentMetaV1Borrowed>(&bytes).unwrap();
            assert!(t.url_list.is_empty(), "{value:?}");
        }
    }

    #[test]
    #[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
    fn test_url_list_does_not_change_info_hash() {
        let without = torrent_from_bytes(&torrent_with_url_list(None))
            .unwrap()
            .info_hash;
        let bytes = torrent_with_url_list(Some(&bstr(b"http://example.com/f")));
        let with = torrent_from_bytes(&bytes).unwrap().info_hash;
        assert_eq!(without, with);
    }

    #[test]
    fn test_url_list_serialize_round_trip() {
        let value = blist(&[bstr(b"http://a.example/"), bstr(b"https://b.example/f")]);
        let bytes = torrent_with_url_list(Some(&value));
        let t = from_bytes::<TorrentMetaV1Borrowed>(&bytes).unwrap();
        let mut out = Vec::new();
        bencode::bencode_serialize_to_writer(&t, &mut out).unwrap();
        let needle = [b"8:url-list".as_slice(), &value].concat();
        assert!(
            out.windows(needle.len()).any(|w| w == needle),
            "{}",
            String::from_utf8_lossy(&out)
        );
        let back = from_bytes::<TorrentMetaV1Borrowed>(&out).unwrap();
        assert_eq!(back.url_list, t.url_list);

        // An empty list is not written.
        let bytes = torrent_with_url_list(None);
        let t = from_bytes::<TorrentMetaV1Borrowed>(&bytes).unwrap();
        let mut out = Vec::new();
        bencode::bencode_serialize_to_writer(&t, &mut out).unwrap();
        assert!(!out.windows(8).any(|w| w == b"url-list"));
    }

    fn bstr(b: &[u8]) -> Vec<u8> {
        let mut o = format!("{}:", b.len()).into_bytes();
        o.extend_from_slice(b);
        o
    }

    fn bint(v: i64) -> Vec<u8> {
        format!("i{v}e").into_bytes()
    }

    fn bdict(entries: &[(&[u8], Vec<u8>)]) -> Vec<u8> {
        let mut o = vec![b'd'];
        for (k, v) in entries {
            o.extend_from_slice(&bstr(k));
            o.extend_from_slice(v);
        }
        o.push(b'e');
        o
    }

    fn harness_multi_named(name: &str, files: &[(&str, u64)]) -> Vec<u8> {
        let mut flist = vec![b'l'];
        for (fname, len) in files {
            let mut path = vec![b'l'];
            path.extend_from_slice(&bstr(fname.as_bytes()));
            path.push(b'e');
            flist.extend_from_slice(&bdict(&[
                (b"length".as_slice(), bint(*len as i64)),
                (b"path".as_slice(), path),
            ]));
        }
        flist.push(b'e');
        harness_multi_with_files(name, flist)
    }

    // Multi-file metainfo with BEP-47 padding entries: each pad is
    // ".pad/<len>" with attr "p".
    fn harness_padded_multi(name: &str, files: &[(&str, u64)], pads: &[(&str, u64)]) -> Vec<u8> {
        let mut flist = vec![b'l'];
        let push = |flist: &mut Vec<u8>, fname: &[&[u8]], len: i64, attr: Option<&[u8]>| {
            let mut path = vec![b'l'];
            for bit in fname {
                path.extend_from_slice(&bstr(bit));
            }
            path.push(b'e');
            let mut entries: Vec<(&[u8], Vec<u8>)> = vec![
                (b"length".as_slice(), bint(len)),
                (b"path".as_slice(), path),
            ];
            if let Some(a) = attr {
                entries.push((b"attr".as_slice(), bstr(a)));
            }
            flist.extend_from_slice(&bdict(&entries));
        };
        for (fname, len) in files {
            push(&mut flist, &[fname.as_bytes()], *len as i64, None);
        }
        for (fname, len) in pads {
            push(
                &mut flist,
                &[b".pad", fname.as_bytes()],
                *len as i64,
                Some(b"p"),
            );
        }
        flist.push(b'e');
        harness_multi_with_files(name, flist)
    }

    fn harness_multi_with_files(name: &str, flist: Vec<u8>) -> Vec<u8> {
        let pieces = vec![0u8; 20];
        let info = bdict(&[
            (b"files".as_slice(), flist),
            (b"name".as_slice(), bstr(name.as_bytes())),
            (b"piece length".as_slice(), bint(16384)),
            (b"pieces".as_slice(), bstr(&pieces)),
        ]);
        let mut top = vec![b'd'];
        top.extend_from_slice(&bstr(b"announce"));
        top.extend_from_slice(&bstr(b"http"));
        top.extend_from_slice(&bstr(b"info"));
        top.extend_from_slice(&info);
        top.push(b'e');
        top
    }
}
