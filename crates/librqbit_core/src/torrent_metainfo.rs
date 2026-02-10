use bencode::WithRawBytes;
use buffers::{ByteBuf, ByteBufOwned};
use bytes::Bytes;
use clone_to_owned::CloneToOwned;
use encoding_rs::Encoding;
use itertools::Either;
use serde_derive::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    collections::{BTreeMap, HashSet},
    iter::once,
    path::PathBuf,
};
use tracing::debug;

use crate::{
    Error,
    hash_id::{Id20, Id32},
    lengths::{Lengths, V2Lengths},
};

pub type TorrentMetaV1Borrowed<'a> = TorrentMetaV1<ByteBuf<'a>>;
pub type TorrentMetaV1Owned = TorrentMetaV1<ByteBufOwned>;

// ============================================================================
// BEP 52 (BitTorrent v2) File Tree Structures
// ============================================================================

/// A single file entry within the v2 file tree.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct V2FileEntry {
    pub length: u64,
    /// SHA-256 merkle root of this file's piece hashes.
    /// Absent for zero-length files.
    #[serde(
        rename = "pieces root",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub pieces_root: Option<Id32>,
}

/// A node in the BEP 52 file tree. Either a directory (containing children)
/// or a file (containing a V2FileEntry at key "").
///
/// In bencode representation:
/// - Directory nodes are dictionaries mapping path component keys to child nodes.
/// - File nodes are dictionaries containing key "" (empty string) which maps to
///   a dictionary with `length` and optionally `pieces root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum V2FileTreeNode<BufType> {
    File(V2FileEntry),
    Directory(BTreeMap<BufType, V2FileTreeNode<BufType>>),
}

impl<BufType> Default for V2FileTreeNode<BufType> {
    fn default() -> Self {
        V2FileTreeNode::Directory(BTreeMap::new())
    }
}

// Custom serde implementation for V2FileTreeNode
// The bencode dict key "" signals a file entry; all other keys are directory children.
mod v2_file_tree_serde {
    use super::*;
    use serde::de::{self, MapAccess, Visitor};
    use serde::ser::SerializeMap;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::fmt;
    use std::marker::PhantomData;

    impl<BufType> Serialize for V2FileTreeNode<BufType>
    where
        BufType: Serialize + AsRef<[u8]>,
    {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            match self {
                V2FileTreeNode::File(entry) => {
                    let mut map = serializer.serialize_map(Some(1))?;
                    map.serialize_entry("", entry)?;
                    map.end()
                }
                V2FileTreeNode::Directory(children) => {
                    let mut map = serializer.serialize_map(Some(children.len()))?;
                    for (key, value) in children {
                        map.serialize_entry(key, value)?;
                    }
                    map.end()
                }
            }
        }
    }

    impl<'de, BufType> Deserialize<'de> for V2FileTreeNode<BufType>
    where
        BufType: Deserialize<'de> + Ord + AsRef<[u8]>,
    {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            struct V2FileTreeNodeVisitor<BufType>(PhantomData<BufType>);

            impl<'de, BufType> Visitor<'de> for V2FileTreeNodeVisitor<BufType>
            where
                BufType: Deserialize<'de> + Ord + AsRef<[u8]>,
            {
                type Value = V2FileTreeNode<BufType>;

                fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                    formatter.write_str("a v2 file tree node (directory or file)")
                }

                fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
                where
                    M: MapAccess<'de>,
                {
                    let mut children: BTreeMap<BufType, V2FileTreeNode<BufType>> = BTreeMap::new();
                    let mut file_entry: Option<V2FileEntry> = None;

                    while let Some(key) = access.next_key::<BufType>()? {
                        if key.as_ref().is_empty() {
                            // Empty string key "" means this is a file entry
                            if file_entry.is_some() {
                                return Err(de::Error::duplicate_field(""));
                            }
                            file_entry = Some(access.next_value()?);
                        } else {
                            // Non-empty key means this is a directory child
                            let value = access.next_value()?;
                            children.insert(key, value);
                        }
                    }

                    // If we have a file entry AND children, that's invalid per BEP 52
                    // (a node can't be both a file and have directory children)
                    if file_entry.is_some() && !children.is_empty() {
                        return Err(de::Error::custom(
                            "v2 file tree node cannot have both file entry and directory children",
                        ));
                    }

                    if let Some(entry) = file_entry {
                        Ok(V2FileTreeNode::File(entry))
                    } else {
                        Ok(V2FileTreeNode::Directory(children))
                    }
                }
            }

            deserializer.deserialize_map(V2FileTreeNodeVisitor(PhantomData))
        }
    }
}

impl CloneToOwned for V2FileEntry {
    type Target = V2FileEntry;

    fn clone_to_owned(&self, _within_buffer: Option<&Bytes>) -> Self::Target {
        V2FileEntry {
            length: self.length,
            pieces_root: self.pieces_root,
        }
    }
}

impl<BufType> CloneToOwned for V2FileTreeNode<BufType>
where
    BufType: CloneToOwned,
    <BufType as CloneToOwned>::Target: Ord,
{
    type Target = V2FileTreeNode<<BufType as CloneToOwned>::Target>;

    fn clone_to_owned(&self, within_buffer: Option<&Bytes>) -> Self::Target {
        match self {
            V2FileTreeNode::File(entry) => {
                V2FileTreeNode::File(entry.clone_to_owned(within_buffer))
            }
            V2FileTreeNode::Directory(children) => {
                let owned_children: BTreeMap<_, _> = children
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone_to_owned(within_buffer),
                            v.clone_to_owned(within_buffer),
                        )
                    })
                    .collect();
                V2FileTreeNode::Directory(owned_children)
            }
        }
    }
}

/// A file extracted from the v2 file tree, with its path components and entry.
#[derive(Debug)]
pub struct V2FileInfo<'a, BufType> {
    /// Path components from root to this file (not including the torrent name).
    pub path: Vec<&'a BufType>,
    /// The file entry with length and pieces_root.
    pub entry: &'a V2FileEntry,
}

impl<BufType> V2FileTreeNode<BufType> {
    /// Recursively collect all files from this node into the output vector.
    /// `current_path` accumulates path components as we descend.
    fn collect_files_recursive<'a>(
        &'a self,
        current_path: &mut Vec<&'a BufType>,
        output: &mut Vec<V2FileInfo<'a, BufType>>,
    ) {
        match self {
            V2FileTreeNode::File(entry) => {
                output.push(V2FileInfo {
                    path: current_path.clone(),
                    entry,
                });
            }
            V2FileTreeNode::Directory(children) => {
                // BEP 52: entries are ordered by raw byte value (BTreeMap handles this)
                for (name, child) in children {
                    current_path.push(name);
                    child.collect_files_recursive(current_path, output);
                    current_path.pop();
                }
            }
        }
    }
}

/// Collect all files from a v2 file_tree root.
/// The file_tree root is a BTreeMap where keys are top-level path components.
pub fn collect_v2_files<'a, BufType>(
    file_tree: &'a BTreeMap<BufType, V2FileTreeNode<BufType>>,
) -> Vec<V2FileInfo<'a, BufType>> {
    let mut output = Vec::new();
    let mut path = Vec::new();
    for (name, node) in file_tree {
        path.push(name);
        node.collect_files_recursive(&mut path, &mut output);
        path.pop();
    }
    output
}

/// Validate v2 file_tree structure per BEP 52 requirements.
/// Checks:
/// - No "." path components (V2FileTreeDotComponent)
/// - No empty path components (BadTorrentEmptyFilename)
/// - No ".." path components (BadTorrentPathTraversal)
fn validate_v2_file_tree<BufType: AsRef<[u8]>>(
    file_tree: &BTreeMap<BufType, V2FileTreeNode<BufType>>,
) -> crate::Result<()> {
    fn validate_node_recursive<BufType: AsRef<[u8]>>(
        node: &V2FileTreeNode<BufType>,
        key: &BufType,
    ) -> crate::Result<()> {
        let key_bytes = key.as_ref();

        // Check for invalid path components
        if key_bytes == b"." {
            return Err(Error::V2FileTreeDotComponent);
        }
        if key_bytes == b".." {
            return Err(Error::BadTorrentPathTraversal);
        }
        if key_bytes.is_empty() {
            // Empty keys at non-file positions are invalid
            // (empty key "" for file entries is handled by deserialization into File variant)
            return Err(Error::BadTorrentEmptyFilename);
        }

        // Check for path separators in component
        use memchr::memchr;
        if memchr(b'/', key_bytes).is_some() || memchr(b'\\', key_bytes).is_some() {
            return Err(Error::BadTorrentSeparatorInName);
        }

        // Recurse into directories
        if let V2FileTreeNode::Directory(children) = node {
            for (child_key, child_node) in children {
                validate_node_recursive(child_node, child_key)?;
            }
        }

        Ok(())
    }

    // Validate each top-level entry
    for (key, node) in file_tree {
        validate_node_recursive(node, key)?;
    }

    Ok(())
}

/// Detected torrent version based on field presence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TorrentVersion {
    /// v1-only torrent: has `pieces` blob, no `file_tree`/`meta_version`.
    V1Only,
    /// v2-only torrent: has `file_tree` and `meta_version=2`, no `pieces`.
    V2Only,
    /// Hybrid torrent: has both v1 (`pieces`) and v2 (`file_tree`, `meta_version=2`) fields.
    Hybrid,
}

pub struct ParsedTorrent<BufType> {
    /// The parsed torrent.
    pub meta: TorrentMetaV1<BufType>,

    /// The raw bytes of the torrent's "info" dict.
    pub info_bytes: BufType,
}

/// Parse torrent metainfo from bytes (includes info_hash and info_hash_v2).
#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
pub fn torrent_from_bytes<'de>(
    buf: &'de [u8],
) -> Result<TorrentMetaV1<ByteBuf<'de>>, bencode::DeserializeError> {
    let mut t: TorrentMetaV1<ByteBuf<'_>> = bencode::from_bytes(buf)
        .inspect_err(|e| tracing::trace!("error deserializing torrent: {e:#}"))
        .map_err(|e| e.into_kind())?;

    let raw_info = t.info.raw_bytes.as_ref();

    // Always compute SHA-1 info hash from the raw info bytes.
    //
    // DESIGN DECISION: We compute SHA-1 even for v2-only torrents.
    // This is a convenience — `info_hash: Id20` stays non-optional, so
    // existing code that needs a 20-byte identifier (session maps, logging,
    // persistence keys) doesn't need Option-handling everywhere. For v2-only
    // torrents the SHA-1 hash is NOT the canonical identifier and MUST NOT
    // be used for DHT, tracker announces, or handshakes (use the truncated
    // SHA-256 instead — see `Id32::truncate_for_dht()`).
    //
    // Version detection does NOT use info_hash to determine v1 presence;
    // it checks `has_v1_fields()` (pieces blob present). So computing
    // SHA-1 here does not cause a v2-only torrent to be misclassified.
    use sha1w::ISha1;
    let mut sha1_digest = sha1w::Sha1::new();
    sha1_digest.update(raw_info);
    t.info_hash = Id20::new(sha1_digest.finish());

    // Compute SHA-256 info hash only if valid v2 metadata is present.
    // Requires BOTH meta_version == 2 AND file_tree to be present.
    // This ensures info_hash_v2 is only set for torrents where version()
    // returns V2Only or Hybrid, avoiding inconsistent state for malformed
    // torrents that have meta_version but no file_tree.
    if t.info.data.meta_version == Some(2) && t.info.data.file_tree.is_some() {
        use sha1w::ISha256;
        let mut sha256_digest = sha1w::Sha256::new();
        sha256_digest.update(raw_info);
        t.info_hash_v2 = Some(Id32::new(sha256_digest.finish()));
    }

    Ok(t)
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// A parsed .torrent file.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(bound(
    serialize = "BufType: serde::Serialize + AsRef<[u8]>",
    deserialize = "BufType: serde::Deserialize<'de> + Ord + AsRef<[u8]>"
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

    #[serde(skip)]
    pub info_hash: Id20,

    // --- BEP 52 (v2) fields ---
    /// SHA-256 info hash. Present for v2 and hybrid torrents.
    /// Computed from the same raw `info` bytes as `info_hash`.
    #[serde(skip)]
    pub info_hash_v2: Option<Id32>,

    /// Piece layers: maps file `pieces_root` -> concatenated 32-byte piece-layer hashes.
    /// Present at the top level of the .torrent dict, outside `info`.
    ///
    /// BEP 52 requirement: For .torrent files containing v2 metadata, `piece layers`
    /// MUST be present. Every file in the v2 `file_tree` whose size exceeds `piece_length`
    /// MUST have an entry keyed by its `pieces_root`. Files with size <= `piece_length`
    /// have only a single piece whose hash IS the `pieces_root`, so they have no entry.
    ///
    /// The field is `Option` at the serde level because:
    /// - v1-only torrents don't have it.
    /// - Magnet-link resolution fetches the `info` dict first; `piece layers` is obtained
    ///   later via hash request/response messages from peers (BEP 52 §5).
    #[serde(
        rename = "piece layers",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub piece_layers: Option<BTreeMap<Id32, BufType>>,
}

impl<BufType> TorrentMetaV1<BufType> {
    pub fn iter_announce(&self) -> impl Iterator<Item = &BufType> {
        if self.announce_list.iter().flatten().next().is_some() {
            return itertools::Either::Left(self.announce_list.iter().flatten());
        }
        itertools::Either::Right(self.announce.iter())
    }
}

impl<BufType: AsRef<[u8]>> TorrentMetaV1<BufType> {
    /// Returns the SHA-1 info hash. Always computed from the raw info dict
    /// bytes for all torrent types (including v2-only) as a convenience for
    /// internal use (session maps, logging, persistence keys).
    ///
    /// **For v2-only torrents, this is NOT the canonical identifier.**
    /// Use `as_id32().unwrap().truncate_for_dht()` for DHT, tracker
    /// announces, and BEP 3 handshakes with v2-only torrents.
    pub fn as_id20(&self) -> Id20 {
        self.info_hash
    }

    /// Returns the v2 (SHA-256) info hash, if this is a v2 or hybrid torrent.
    pub fn as_id32(&self) -> Option<Id32> {
        self.info_hash_v2
    }

    /// True if the info dict contains v1 fields (`pieces` present and non-empty).
    ///
    /// NOTE: Do NOT use `info_hash != Id20::default()` for this check —
    /// the SHA-1 info hash is always computed from the raw info dict bytes,
    /// so it will be non-default even for v2-only torrents. Instead, check
    /// for the actual presence of v1-specific fields.
    fn has_v1_fields(&self) -> bool {
        self.info
            .data
            .pieces
            .as_ref()
            .is_some_and(|p| !p.as_ref().is_empty())
    }

    /// True if the info dict contains v2 fields (`meta_version == 2` and
    /// `file_tree` present).
    fn has_v2_fields(&self) -> bool {
        self.info.data.meta_version == Some(2) && self.info.data.file_tree.is_some()
    }

    /// True if this torrent has v2 metadata (v2-only or hybrid).
    pub fn is_v2(&self) -> bool {
        self.has_v2_fields()
    }

    /// True if this torrent has v1 metadata only.
    pub fn is_v1_only(&self) -> bool {
        self.has_v1_fields() && !self.has_v2_fields()
    }

    /// True if this torrent has both v1 and v2 metadata (hybrid).
    ///
    /// Hybrid means the info dict contains BOTH the v1 `pieces` blob AND
    /// the v2 `file_tree` + `meta_version == 2`. This is the only reliable
    /// way to detect hybrids — checking `info_hash` is not sufficient
    /// because SHA-1 is always computed from the info dict raw bytes.
    pub fn is_hybrid(&self) -> bool {
        self.has_v1_fields() && self.has_v2_fields()
    }

    /// True if this torrent is v2-only (no v1 pieces blob).
    pub fn is_v2_only(&self) -> bool {
        self.has_v2_fields() && !self.has_v1_fields()
    }

    /// Detect torrent version based on field presence.
    pub fn version(&self) -> Option<TorrentVersion> {
        match (self.has_v1_fields(), self.has_v2_fields()) {
            (true, false) => Some(TorrentVersion::V1Only),
            (false, true) => Some(TorrentVersion::V2Only),
            (true, true) => Some(TorrentVersion::Hybrid),
            (false, false) => None, // Invalid torrent
        }
    }

    /// Validate v2 piece_layers for torrents loaded from .torrent files.
    ///
    /// BEP 52 requirements:
    /// - For v2/hybrid .torrent files, `piece_layers` MUST be present.
    /// - Files with size > piece_length MUST have an entry keyed by their `pieces_root`.
    /// - Files with size <= piece_length MUST NOT have an entry (their hash IS the pieces_root).
    /// - Each entry must have the correct size: ceil(file_size / piece_length) * 32 bytes.
    ///
    /// Note: For magnet link resolution, piece_layers may not be present initially
    /// (obtained later via hash request/response). Use `validate_v2_piece_layers_if_present`
    /// for that case.
    pub fn validate_v2_piece_layers(&self) -> crate::Result<()> {
        // Only validate for v2/hybrid torrents
        if !self.is_v2() {
            return Ok(());
        }

        let piece_layers = self
            .piece_layers
            .as_ref()
            .ok_or(Error::V2MissingPieceLayers)?;

        self.validate_v2_piece_layers_inner(piece_layers)
    }

    /// Validate v2 piece_layers if present (for magnet link resolution where
    /// piece_layers may be obtained later).
    pub fn validate_v2_piece_layers_if_present(&self) -> crate::Result<()> {
        if !self.is_v2() {
            return Ok(());
        }

        if let Some(ref piece_layers) = self.piece_layers {
            self.validate_v2_piece_layers_inner(piece_layers)?;
        }

        Ok(())
    }

    fn validate_v2_piece_layers_inner(
        &self,
        piece_layers: &BTreeMap<Id32, BufType>,
    ) -> crate::Result<()> {
        let file_tree = self
            .info
            .data
            .file_tree
            .as_ref()
            .ok_or(Error::V2MissingFileTree)?;

        let piece_length = self.info.data.piece_length as u64;
        let v2_files = collect_v2_files(file_tree);

        for file_info in &v2_files {
            let file_size = file_info.entry.length;

            if file_size == 0 {
                // Zero-length files have no pieces_root and no piece_layers entry
                if file_info.entry.pieces_root.is_some() {
                    return Err(Error::V2ZeroLengthFileHasPiecesRoot);
                }
                continue;
            }

            if file_size <= piece_length {
                // Small files: pieces_root IS the single piece hash, no piece_layers entry
                if file_info.entry.pieces_root.is_none() {
                    return Err(Error::V2SmallFileMissingPiecesRoot);
                }
                if file_info
                    .entry
                    .pieces_root
                    .as_ref()
                    .is_some_and(|pr| piece_layers.contains_key(pr))
                {
                    return Err(Error::V2SmallFileShouldNotHavePieceLayers);
                }
            } else {
                // Large files: must have piece_layers entry
                let pieces_root = file_info.entry.pieces_root.as_ref().ok_or_else(|| {
                    Error::V2MissingPieceLayersEntry("file missing pieces_root".into())
                })?;

                let layer_data = piece_layers.get(pieces_root).ok_or_else(|| {
                    let path: Vec<String> = file_info
                        .path
                        .iter()
                        .map(|p| String::from_utf8_lossy(p.as_ref()).into_owned())
                        .collect();
                    Error::V2MissingPieceLayersEntry(path.join("/"))
                })?;

                // Validate piece_layers entry size
                let num_pieces = file_size.div_ceil(piece_length) as usize;
                let expected_size = num_pieces * 32;
                let actual_size = layer_data.as_ref().len();
                if actual_size != expected_size {
                    return Err(Error::V2PieceLayersWrongSize {
                        expected: expected_size,
                        actual: actual_size,
                    });
                }

                // Verify merkle root: rebuild from piece layer hashes and compare
                // against the file's pieces_root.
                #[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
                {
                    let piece_hashes: Vec<Id32> = layer_data
                        .as_ref()
                        .chunks_exact(32)
                        .map(|c| Id32::new(c.try_into().unwrap()))
                        .collect();
                    let computed_root = crate::merkle::root_from_piece_layer(
                        &piece_hashes,
                        file_size,
                        self.info.data.piece_length,
                    )?;
                    if computed_root != *pieces_root {
                        return Err(Error::V2PieceLayersRootMismatch);
                    }
                }
            }
        }

        Ok(())
    }
}

/// Main torrent information, shared by .torrent files and magnet link contents.
#[derive(Default, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(bound(
    serialize = "BufType: serde::Serialize + AsRef<[u8]>",
    deserialize = "BufType: serde::Deserialize<'de> + Ord + AsRef<[u8]>"
))]
pub struct TorrentMetaV1Info<BufType> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<BufType>,

    /// v1 piece hashes (concatenated 20-byte SHA-1 digests).
    /// CHANGED from `BufType` to `Option<BufType>`:
    ///   - Present for v1-only and hybrid torrents.
    ///   - Absent (None) for v2-only torrents (BEP 52 info dict omits this key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

    // Multi-file mode (v1)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<TorrentMetaV1File<BufType>>>,

    #[serde(skip_serializing_if = "is_false", default)]
    pub private: bool,

    // --- BEP 52 (v2) fields within info dict ---
    /// BEP 52 meta version. If present and == 2, this is a v2 or hybrid torrent.
    #[serde(
        rename = "meta version",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub meta_version: Option<u32>,

    /// BEP 52 file tree. Present in v2 and hybrid torrents.
    /// Replaces the v1 `files` list with a nested directory structure.
    #[serde(rename = "file tree", default, skip_serializing_if = "Option::is_none")]
    pub file_tree: Option<BTreeMap<BufType, V2FileTreeNode<BufType>>>,
}

#[derive(Clone)]
pub struct FileIteratorName<'a, BufType> {
    encoding: &'static Encoding,
    data: FileIteratorNameData<'a, BufType>,
}

#[derive(Clone)]
pub enum FileIteratorNameData<'a, BufType> {
    Single(Option<&'a BufType>),
    Tree(&'a [BufType]),
    /// v2 file tree path: a vec of references to path components
    V2Tree(Vec<&'a BufType>),
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
    pub fn iter_components(&self) -> impl Iterator<Item = Cow<'a, str>> + '_ {
        let encoding = self.encoding;
        self.iter_components_bytes()
            .map(move |part| encoding.decode(part).0)
    }

    /// Iterate path components as bytes.
    pub fn iter_components_bytes(&self) -> impl Iterator<Item = &'a [u8]> + '_ {
        match &self.data {
            FileIteratorNameData::Single(None) => {
                Either::Left(Either::Left(once(&b"torrent-content"[..])))
            }
            FileIteratorNameData::Single(Some(name)) => {
                Either::Left(Either::Right(once((*name).as_ref())))
            }
            FileIteratorNameData::Tree(t) => {
                Either::Right(Either::Left(t.iter().map(|bb| bb.as_ref())))
            }
            FileIteratorNameData::V2Tree(t) => {
                Either::Right(Either::Right(t.iter().map(|bb| (*bb).as_ref())))
            }
        }
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
    v2_lengths: Option<V2Lengths>,
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

    pub fn v2_lengths(&self) -> Option<&V2Lengths> {
        self.v2_lengths.as_ref()
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
        let v2_files = self.v2_lengths.as_ref().map(|v2| v2.files());
        self.iter_file_details()
            .enumerate()
            .scan(0u64, move |acc_offset, (file_idx, details)| {
                let offset = *acc_offset;
                *acc_offset += details.len;

                let pieces = if let Some(v2_files) = v2_files {
                    // v2: use file-aligned piece ranges from V2Lengths.
                    if let Some(fi) = v2_files.get(file_idx) {
                        fi.first_piece_index..(fi.first_piece_index + fi.num_pieces)
                    } else {
                        0..0
                    }
                } else {
                    // v1: use concatenated offset model.
                    self.lengths.iter_pieces_within_offset(offset, details.len)
                };

                Some(FileDetailsExt {
                    pieces,
                    details,
                    offset,
                })
            })
    }
}

impl<BufType: AsRef<[u8]>> TorrentMetaV1Info<BufType> {
    pub fn validate(self) -> crate::Result<ValidatedTorrentMetaV1Info<BufType>> {
        // ====================================================================
        // BEP 52 (v2) validation
        // ====================================================================

        // Check meta_version consistency
        if let Some(version) = self.meta_version {
            if version != 2 {
                return Err(Error::V2UnsupportedMetaVersion(version));
            }
            // meta_version == 2 requires file_tree
            if self.file_tree.is_none() {
                return Err(Error::V2MissingFileTree);
            }
        }

        // Check file_tree consistency
        if let Some(ref file_tree) = self.file_tree {
            // file_tree requires meta_version == 2
            if self.meta_version != Some(2) {
                return Err(Error::V2MissingMetaVersion);
            }
            // Validate file_tree structure (path components, traversal, etc.)
            validate_v2_file_tree(file_tree)?;
        }

        // v2 piece length constraints (BEP 52): power of two, >= 16 KiB.
        if self.meta_version == Some(2) || self.file_tree.is_some() {
            let piece_length = self.piece_length;
            if piece_length < crate::merkle::MERKLE_BLOCK_SIZE || !piece_length.is_power_of_two() {
                return Err(Error::V2InvalidPieceLength(piece_length));
            }
        }

        // Ensure we have either v1 or v2 file information
        let has_v1_files = self.length.is_some() || self.files.is_some();
        let has_v2_files = self.file_tree.is_some();
        if !has_v1_files && !has_v2_files {
            return Err(Error::BadTorrentNoFiles);
        }

        // If meta_version is set but we have neither pieces (v1) nor file_tree (v2),
        // the torrent is invalid
        if self.meta_version.is_some()
            && self.pieces.as_ref().is_none_or(|p| p.as_ref().is_empty())
            && self.file_tree.is_none()
        {
            return Err(Error::V2InvalidTorrent);
        }

        // ====================================================================
        // Common validation (v1 and v2)
        // ====================================================================

        let encoding = self.detect_encoding();

        let has_v1_fields = self.pieces.as_ref().is_some_and(|p| !p.as_ref().is_empty());
        let has_v2_fields = self.meta_version == Some(2) && self.file_tree.is_some();
        let is_v2_only = has_v2_fields && !has_v1_fields;

        // Hybrid consistency: v1 real files (excluding padding) must match v2 file_tree.
        if has_v1_fields && has_v2_fields {
            let file_tree = self.file_tree.as_ref().unwrap();
            let v2_files = collect_v2_files(file_tree);
            let mut v1_files: Vec<(Vec<Vec<u8>>, u64)> = Vec::new();
            for file in self.iter_file_details_raw(encoding)? {
                if file.attrs().padding {
                    continue;
                }
                let path: Vec<Vec<u8>> = file
                    .filename
                    .iter_components_bytes()
                    .map(|p| p.to_vec())
                    .collect();
                v1_files.push((path, file.len));
            }

            if v1_files.len() != v2_files.len() {
                return Err(Error::V2HybridFileListMismatch(format!(
                    "file count mismatch: v1_real_files={} v2_files={}",
                    v1_files.len(),
                    v2_files.len()
                )));
            }

            for (idx, v2_file) in v2_files.iter().enumerate() {
                let (v1_path, v1_len) = &v1_files[idx];
                let v2_path: Vec<Vec<u8>> =
                    v2_file.path.iter().map(|p| p.as_ref().to_vec()).collect();
                if *v1_len != v2_file.entry.length || *v1_path != v2_path {
                    return Err(Error::V2HybridFileListMismatch(format!(
                        "file {} mismatch: v1_len={} v2_len={} v1_path={:?} v2_path={:?}",
                        idx, v1_len, v2_file.entry.length, v1_path, v2_path
                    )));
                }
            }
        }

        // Build V2Lengths from v2 file_tree (if present).
        let v2_lengths = if has_v2_fields {
            let file_tree = self.file_tree.as_ref().unwrap();
            let v2_files = collect_v2_files(file_tree);
            let file_lengths: Vec<u64> = v2_files.iter().map(|f| f.entry.length).collect();
            Some(V2Lengths::try_new(self.piece_length, &file_lengths)?)
        } else {
            None
        };

        // Build Lengths based on torrent version.
        let lengths = if is_v2_only {
            Lengths::from_v2(
                v2_lengths
                    .as_ref()
                    .expect("v2-only torrents must have v2_lengths"),
            )?
        } else {
            // v1 or hybrid: use existing v1 concatenated model.
            Lengths::from_torrent(&self)?
        };

        let validated = ValidatedTorrentMetaV1Info {
            encoding,
            lengths,
            info: self,
            v2_lengths,
        };

        // Ensure:
        // - there's at least one file
        // - each filename has at least one path component
        // - there's no path traversal
        // - all filenames are unique
        let mut seen_files = 0;
        for file in validated.info.iter_file_details_raw(encoding)? {
            seen_files += 1;
            let mut seen_a_bit = false;
            for bit in file.filename.iter_components_bytes() {
                seen_a_bit = true;
                if bit == b".." {
                    return Err(Error::BadTorrentPathTraversal);
                }
                // BEP 52: "." is also forbidden in v2 file trees
                // (already checked in validate_v2_file_tree, but check here for v1 too)
                if bit == b"." {
                    return Err(Error::V2FileTreeDotComponent);
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

        let mut unique_filenames = HashSet::<PathBuf>::new();
        for fd in validated.iter_file_details() {
            let pb = fd.filename.to_pathbuf();
            if pb.as_os_str().is_empty() {
                return Err(Error::BadTorrentFileNoName);
            }
            unique_filenames.insert(pb);
        }
        if unique_filenames.len() != seen_files {
            return Err(Error::BadTorrentDuplicateFilenames);
        }

        Ok(validated)
    }

    /// Get the v1 SHA-1 hash for a piece. Returns None if:
    /// - This is a v2-only torrent (no pieces blob)
    /// - The piece index is out of range
    pub fn get_hash(&self, piece: u32) -> Option<&[u8]> {
        let pieces = self.pieces.as_ref()?;
        let start = piece as usize * 20;
        let end = start + 20;
        let expected_hash = pieces.as_ref().get(start..end)?;
        Some(expected_hash)
    }

    /// Compare a computed hash against the expected v1 SHA-1 hash for a piece.
    /// Returns None if this is a v2-only torrent or piece index is out of range.
    pub fn compare_hash(&self, piece: u32, hash: [u8; 20]) -> Option<bool> {
        let pieces = self.pieces.as_ref()?;
        let start = piece as usize * 20;
        let end = start + 20;
        let expected_hash = pieces.as_ref().get(start..end)?;
        Some(expected_hash == hash)
    }

    pub fn detect_encoding(&self) -> &'static Encoding {
        let mut encdetect = chardetng::EncodingDetector::new();
        if let Some(name) = self.name.as_ref() {
            encdetect.feed(name.as_ref(), false);
        }

        // v1 multi-file paths
        for file in self.files.iter().flat_map(|f| f.iter()) {
            for component in file.path.iter() {
                encdetect.feed(component.as_ref(), false);
            }
        }

        // v2 file_tree paths
        if let Some(file_tree) = self.file_tree.as_ref() {
            for file_info in collect_v2_files(file_tree) {
                for component in file_info.path {
                    encdetect.feed(component.as_ref(), false);
                }
            }
        }

        encdetect.guess(None, true)
    }

    pub(crate) fn iter_file_details_raw(
        &self,
        encoding: &'static Encoding,
    ) -> crate::Result<impl Iterator<Item = FileDetails<'_, BufType>>> {
        // Priority: v1 single-file > v1 multi-file > v2 file_tree
        // This ensures hybrid torrents use v1 layout for backward compatibility.
        match (self.length, self.files.as_ref(), self.file_tree.as_ref()) {
            // v1 Single-file mode
            (Some(length), None, _) => Ok(Either::Left(Either::Left(once(FileDetails {
                filename: FileIteratorName {
                    encoding,
                    data: FileIteratorNameData::Single(self.name.as_ref()),
                },
                len: length,
                attr: self.attr.as_ref(),
                sha1: self.sha1.as_ref(),
                symlink_path: self.symlink_path.as_deref(),
            })))),

            // v1 Multi-file mode
            (None, Some(files), _) => {
                if files.is_empty() {
                    return Err(Error::BadTorrentMultiFileEmpty);
                }
                Ok(Either::Left(Either::Right(files.iter().map(move |f| {
                    FileDetails {
                        filename: FileIteratorName {
                            encoding,
                            data: FileIteratorNameData::Tree(&f.path),
                        },
                        len: f.length,
                        attr: f.attr.as_ref(),
                        sha1: f.sha1.as_ref(),
                        symlink_path: f.symlink_path.as_deref(),
                    }
                }))))
            }

            // v2-only: use file_tree
            (None, None, Some(file_tree)) => {
                let v2_files = collect_v2_files(file_tree);
                if v2_files.is_empty() {
                    return Err(Error::BadTorrentNoFiles);
                }
                // Convert to owned Vec of FileDetails to return as iterator
                let details: Vec<FileDetails<'_, BufType>> = v2_files
                    .into_iter()
                    .map(|f| FileDetails {
                        filename: FileIteratorName {
                            encoding,
                            data: FileIteratorNameData::V2Tree(f.path),
                        },
                        len: f.entry.length,
                        // v2 doesn't have these fields per-file in the same way
                        attr: None,
                        sha1: None,
                        symlink_path: None,
                    })
                    .collect();
                Ok(Either::Right(details.into_iter()))
            }

            // Invalid: has both single-file length and multi-file files
            (Some(_), Some(_), _) => Err(Error::BadTorrentBothSingleAndMultiFile),

            // Invalid: no file information at all
            (None, None, None) => Err(Error::BadTorrentNoFiles),
        }
    }
}

const fn none<T>() -> Option<T> {
    None
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct TorrentMetaV1File<BufType> {
    pub length: u64,
    pub path: Vec<BufType>,

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
            attr: self.attr.clone_to_owned(within_buffer),
            sha1: self.sha1.clone_to_owned(within_buffer),
            symlink_path: self.symlink_path.clone_to_owned(within_buffer),
        }
    }
}

impl<BufType> CloneToOwned for TorrentMetaV1Info<BufType>
where
    BufType: CloneToOwned,
    <BufType as CloneToOwned>::Target: Ord,
{
    type Target = TorrentMetaV1Info<<BufType as CloneToOwned>::Target>;

    fn clone_to_owned(&self, within_buffer: Option<&Bytes>) -> Self::Target {
        TorrentMetaV1Info {
            name: self.name.clone_to_owned(within_buffer),
            pieces: self.pieces.clone_to_owned(within_buffer),
            piece_length: self.piece_length,
            length: self.length,
            md5sum: self.md5sum.clone_to_owned(within_buffer),
            files: self.files.clone_to_owned(within_buffer),
            attr: self.attr.clone_to_owned(within_buffer),
            sha1: self.sha1.clone_to_owned(within_buffer),
            symlink_path: self.symlink_path.clone_to_owned(within_buffer),
            private: self.private,
            // v2 fields
            meta_version: self.meta_version,
            file_tree: self.file_tree.clone_to_owned(within_buffer),
        }
    }
}

impl<BufType> CloneToOwned for TorrentMetaV1<BufType>
where
    BufType: CloneToOwned,
    <BufType as CloneToOwned>::Target: Ord,
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
            info_hash: self.info_hash,
            // v2 fields
            info_hash_v2: self.info_hash_v2,
            piece_layers: self.piece_layers.clone_to_owned(within_buffer),
        }
    }
}

#[cfg(test)]
mod tests {
    use bencode::{BencodeValue, from_bytes};
    use bytes::Bytes;

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

    #[test]
    fn test_v2_small_file_requires_pieces_root() {
        let mut file_tree = BTreeMap::new();
        file_tree.insert(
            ByteBufOwned(Bytes::from_static(b"file.txt")),
            V2FileTreeNode::File(V2FileEntry {
                length: 1,
                pieces_root: None,
            }),
        );

        let info = TorrentMetaV1Info {
            name: None,
            pieces: None,
            piece_length: crate::merkle::MERKLE_BLOCK_SIZE,
            length: None,
            attr: None,
            sha1: None,
            symlink_path: None,
            md5sum: None,
            files: None,
            private: false,
            meta_version: Some(2),
            file_tree: Some(file_tree),
        };

        let torrent = TorrentMetaV1 {
            announce: None,
            announce_list: Vec::new(),
            info: WithRawBytes {
                data: info,
                raw_bytes: ByteBufOwned(Bytes::new()),
            },
            comment: None,
            created_by: None,
            encoding: None,
            publisher: None,
            publisher_url: None,
            creation_date: None,
            info_hash: Id20::default(),
            info_hash_v2: None,
            piece_layers: Some(BTreeMap::new()),
        };

        let err = torrent.validate_v2_piece_layers().unwrap_err();
        assert!(matches!(err, Error::V2SmallFileMissingPiecesRoot));
    }

    #[test]
    fn test_v2_empty_file_with_pieces_root_is_invalid() {
        let mut file_tree = BTreeMap::new();
        file_tree.insert(
            ByteBufOwned(Bytes::from_static(b"empty.bin")),
            V2FileTreeNode::File(V2FileEntry {
                length: 0,
                pieces_root: Some(Id32::new([0u8; 32])),
            }),
        );

        let info = TorrentMetaV1Info {
            name: None,
            pieces: None,
            piece_length: crate::merkle::MERKLE_BLOCK_SIZE,
            length: None,
            attr: None,
            sha1: None,
            symlink_path: None,
            md5sum: None,
            files: None,
            private: false,
            meta_version: Some(2),
            file_tree: Some(file_tree),
        };

        let torrent = TorrentMetaV1 {
            announce: None,
            announce_list: Vec::new(),
            info: WithRawBytes {
                data: info,
                raw_bytes: ByteBufOwned(Bytes::new()),
            },
            comment: None,
            created_by: None,
            encoding: None,
            publisher: None,
            publisher_url: None,
            creation_date: None,
            info_hash: Id20::default(),
            info_hash_v2: None,
            piece_layers: Some(BTreeMap::new()),
        };

        let err = torrent.validate_v2_piece_layers().unwrap_err();
        assert!(matches!(err, Error::V2ZeroLengthFileHasPiecesRoot));
    }

    #[test]
    fn test_v2_invalid_piece_length_rejected() {
        let mut file_tree = BTreeMap::new();
        file_tree.insert(
            ByteBufOwned(Bytes::from_static(b"file.bin")),
            V2FileTreeNode::File(V2FileEntry {
                length: 1,
                pieces_root: Some(Id32::new([1u8; 32])),
            }),
        );

        let info = TorrentMetaV1Info {
            name: None,
            pieces: None,
            piece_length: 8192, // < 16 KiB and not allowed for v2
            length: None,
            attr: None,
            sha1: None,
            symlink_path: None,
            md5sum: None,
            files: None,
            private: false,
            meta_version: Some(2),
            file_tree: Some(file_tree),
        };

        let err = info.validate().unwrap_err();
        assert!(matches!(err, Error::V2InvalidPieceLength(8192)));
    }

    // =====================================================================
    // BEP 52 (v2) integration tests using libtorrent test vectors
    // =====================================================================

    #[test]
    #[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
    fn test_v2_only_torrent_parse_and_validate() {
        let buf = include_bytes!("resources/test/v2_only.torrent");
        let torrent = torrent_from_bytes(buf).unwrap();

        // Should be v2-only.
        assert!(torrent.is_v2_only(), "expected v2-only torrent");
        assert!(!torrent.is_v1_only());
        assert!(!torrent.is_hybrid());
        assert!(torrent.is_v2());
        assert_eq!(torrent.version(), Some(TorrentVersion::V2Only));

        // Should have info_hash_v2.
        assert!(
            torrent.info_hash_v2.is_some(),
            "v2-only torrent should have info_hash_v2"
        );

        // info dict should have meta_version == 2 and file_tree.
        assert_eq!(torrent.info.data.meta_version, Some(2));
        assert!(torrent.info.data.file_tree.is_some());

        // pieces should be absent.
        assert!(
            torrent.info.data.pieces.is_none()
                || torrent
                    .info
                    .data
                    .pieces
                    .as_ref()
                    .is_some_and(|p| p.as_ref().is_empty()),
            "v2-only should not have pieces blob"
        );

        // File tree should have at least one file.
        let file_tree = torrent.info.data.file_tree.as_ref().unwrap();
        let files = collect_v2_files(file_tree);
        assert!(!files.is_empty(), "should have at least one file");

        // Validate the info dict.
        let validated = torrent.info.data.validate().unwrap();
        assert!(
            validated.v2_lengths().is_some(),
            "v2-only torrent should have v2_lengths"
        );
    }

    #[test]
    #[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
    fn test_v2_hybrid_torrent_parse_and_validate() {
        let buf = include_bytes!("resources/test/v2_hybrid.torrent");
        let torrent = torrent_from_bytes(buf).unwrap();

        // Should be hybrid.
        assert!(torrent.is_hybrid(), "expected hybrid torrent");
        assert!(!torrent.is_v1_only());
        assert!(!torrent.is_v2_only());
        assert!(torrent.is_v2());
        assert_eq!(torrent.version(), Some(TorrentVersion::Hybrid));

        // Should have both hashes.
        assert!(
            torrent.info_hash_v2.is_some(),
            "hybrid torrent should have info_hash_v2"
        );
        assert_ne!(
            torrent.info_hash,
            Id20::default(),
            "should have non-default info_hash"
        );

        // Should have both v1 and v2 fields.
        assert!(
            torrent.info.data.pieces.is_some(),
            "hybrid should have pieces blob"
        );
        assert_eq!(torrent.info.data.meta_version, Some(2));
        assert!(
            torrent.info.data.file_tree.is_some(),
            "hybrid should have file_tree"
        );

        // Validate the info dict.
        let _validated = torrent.info.data.validate().unwrap();
    }

    #[test]
    #[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
    fn test_v2_hybrid_file_list_mismatch_is_rejected() {
        let buf = include_bytes!("resources/test/v2_hybrid.torrent");
        let torrent = torrent_from_bytes(buf).unwrap();

        let mut info = torrent.info.data.clone();
        if let Some(files) = info.files.as_mut() {
            let mut changed = false;
            for file in files {
                if file.attr.as_ref().is_some_and(|a| a.as_ref() == b"p") {
                    continue;
                }
                file.length = file.length.saturating_add(1);
                changed = true;
                break;
            }
            assert!(changed, "expected at least one non-padding v1 file");
        } else if let Some(length) = info.length.as_mut() {
            *length = length.saturating_add(1);
        } else {
            panic!("hybrid torrent missing v1 file info");
        }

        let err = info.validate().unwrap_err();
        assert!(matches!(err, Error::V2HybridFileListMismatch(_)));
    }

    #[test]
    #[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
    fn test_v2_multipiece_torrent_parse_and_validate() {
        let buf = include_bytes!("resources/test/v2_multipiece_file.torrent");
        let torrent = torrent_from_bytes(buf).unwrap();

        // Should be v2 (v2-only or hybrid).
        assert!(torrent.is_v2(), "expected v2 torrent");

        // Should have piece_layers for multi-piece files.
        assert!(
            torrent.piece_layers.is_some(),
            "multi-piece v2 torrent should have piece_layers"
        );

        let piece_layers = torrent.piece_layers.as_ref().unwrap();
        assert!(
            !piece_layers.is_empty(),
            "piece_layers should not be empty for multi-piece file"
        );

        // Validate piece_layers (includes merkle root verification).
        torrent
            .validate_v2_piece_layers()
            .expect("piece_layers validation should pass for valid torrent");

        // Validate the info dict.
        let is_v2_only = torrent.is_v2_only();
        let validated = torrent.info.data.validate().unwrap();
        if is_v2_only {
            assert!(validated.v2_lengths().is_some());
        }
    }

    #[test]
    #[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
    fn test_v2_multiple_files_torrent() {
        let buf = include_bytes!("resources/test/v2_multiple_files.torrent");
        let torrent = torrent_from_bytes(buf).unwrap();

        assert!(torrent.is_v2(), "expected v2 torrent");

        let file_tree = torrent.info.data.file_tree.as_ref().unwrap();
        let files = collect_v2_files(file_tree);
        assert!(
            files.len() > 1,
            "should have multiple files, got {}",
            files.len()
        );

        // Validate piece_layers if present.
        if torrent.piece_layers.is_some() {
            torrent
                .validate_v2_piece_layers()
                .expect("piece_layers validation should pass");
        }

        // Validate info dict.
        let _validated = torrent.info.data.validate().unwrap();
    }

    #[test]
    #[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
    fn test_v2_empty_file_torrent() {
        let buf = include_bytes!("resources/test/v2_empty_file.torrent");
        let torrent = torrent_from_bytes(buf).unwrap();

        assert!(torrent.is_v2(), "expected v2 torrent");

        // Should have at least one file with length 0.
        let file_tree = torrent.info.data.file_tree.as_ref().unwrap();
        let files = collect_v2_files(file_tree);
        let has_empty = files.iter().any(|f| f.entry.length == 0);
        assert!(has_empty, "should contain a zero-length file");

        // Empty files should not have pieces_root.
        for f in &files {
            if f.entry.length == 0 {
                assert!(
                    f.entry.pieces_root.is_none(),
                    "zero-length file should not have pieces_root"
                );
            }
        }

        // Validate piece_layers if present.
        if torrent.piece_layers.is_some() {
            torrent
                .validate_v2_piece_layers()
                .expect("piece_layers validation should pass");
        }
    }
}
