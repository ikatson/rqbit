//! The BitTorrent v2 (BEP 52) parts of the metainfo: the `file tree` of the
//! info dictionary, the `piece layers` of the torrent file, the per-file
//! layout derived from them, and the piece verifier.

use std::{
    fmt,
    marker::PhantomData,
    sync::{Arc, OnceLock},
};

use bytes::Bytes;
use clone_to_owned::CloneToOwned;
// The traits come from their modules: the crate root also re-exports the
// derive macros when serde's `derive` feature is on, which would clash with
// the serde_derive import below.
use serde::{
    Deserializer, Serializer,
    de::{Deserialize, DeserializeSeed, IgnoredAny, MapAccess, Visitor},
    ser::{Serialize, SerializeMap},
};
use serde_derive::{Deserialize, Serialize};

use crate::hash_id::Id32;

/// Deepest directory nesting accepted in a `file tree` (the same limit as
/// libtorrent). Deeper trees are rejected by validation.
pub const MAX_FILE_TREE_DEPTH: usize = 100;

/// Most files accepted in a `file tree`. Larger trees are rejected by
/// validation.
pub const MAX_FILE_TREE_FILES: usize = 1 << 20;

/// Most path components accepted in a `file tree`, summed over its files.
/// Every file stores its full path, so without this bound a few kilobytes of
/// deep directories shared by many files would expand into gigabytes.
pub const MAX_FILE_TREE_PATH_COMPONENTS: usize = 1 << 23;

/// The version of a torrent's metainfo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TorrentVersion {
    /// BEP 3: SHA-1 piece hashes in `pieces`.
    V1,
    /// BEP 52 only: per-file merkle trees, no `pieces`.
    V2,
    /// Both: the v1 and the v2 descriptions of the same, piece-aligned data.
    Hybrid,
}

impl TorrentVersion {
    /// True if the torrent has v1 (SHA-1) piece hashes.
    pub fn has_v1(self) -> bool {
        matches!(self, TorrentVersion::V1 | TorrentVersion::Hybrid)
    }

    /// True if the torrent has v2 (merkle) hashes.
    pub fn has_v2(self) -> bool {
        matches!(self, TorrentVersion::V2 | TorrentVersion::Hybrid)
    }
}

/// Something about a `file tree` that broke a rule while parsing. The parser
/// keeps going (so the info hash can still be computed) and validation
/// reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileTreeProblem {
    TooDeep,
    TooManyFiles,
    TooManyPathComponents,
    MixedNode,
    RootIsFile,
}

/// The files of a BEP 52 `file tree`, flattened in depth-first order of the
/// dictionary keys (which is the order of the files in the torrent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V2FileTree<BufType> {
    /// The files in torrent order.
    pub files: Vec<V2FileTreeEntry<BufType>>,
    pub(crate) problem: Option<FileTreeProblem>,
}

impl<BufType> Default for V2FileTree<BufType> {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl<BufType> V2FileTree<BufType> {
    /// A file tree with the given files, in torrent order (depth-first, sorted
    /// by path component bytes at every level).
    pub fn new(files: Vec<V2FileTreeEntry<BufType>>) -> Self {
        Self {
            files,
            problem: None,
        }
    }
}

/// One file of a BEP 52 `file tree`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V2FileTreeEntry<BufType> {
    /// Path components, relative to the torrent's root directory.
    pub path: Vec<BufType>,
    /// Length in bytes.
    pub length: u64,
    /// Root of the file's merkle tree; absent for empty files. Kept raw so
    /// that validation can report a wrong length.
    pub pieces_root: Option<BufType>,
    /// BEP 47 attributes.
    pub attr: Option<BufType>,
}

impl<BufType: AsRef<[u8]>> V2FileTreeEntry<BufType> {
    /// The pieces root, if present and 32 bytes long.
    pub fn pieces_root_id(&self) -> Option<Id32> {
        self.pieces_root
            .as_ref()
            .and_then(|r| Id32::from_bytes(r.as_ref()).ok())
    }
}

const fn none<T>() -> Option<T> {
    None
}

#[derive(Deserialize)]
struct FileProps<BufType> {
    length: u64,
    #[serde(rename = "pieces root", default = "none")]
    pieces_root: Option<BufType>,
    #[serde(default = "none")]
    attr: Option<BufType>,
}

#[derive(Serialize)]
struct FilePropsRef<'a, BufType> {
    #[serde(skip_serializing_if = "Option::is_none")]
    attr: Option<&'a BufType>,
    length: u64,
    #[serde(rename = "pieces root", skip_serializing_if = "Option::is_none")]
    pieces_root: Option<&'a BufType>,
}

/// Bounds on what a `file tree` may hold, see [`MAX_FILE_TREE_DEPTH`],
/// [`MAX_FILE_TREE_FILES`] and [`MAX_FILE_TREE_PATH_COMPONENTS`].
#[derive(Clone, Copy)]
struct TreeLimits {
    depth: usize,
    files: usize,
    path_components: usize,
}

/// Visits one directory dictionary of the tree, appending its files.
struct NodeSeed<'a, BufType> {
    prefix: &'a mut Vec<BufType>,
    tree: &'a mut V2FileTree<BufType>,
    // Path components stored so far, summed over the files.
    path_components: &'a mut usize,
    limits: TreeLimits,
}

impl<'de, BufType> DeserializeSeed<'de> for NodeSeed<'_, BufType>
where
    BufType: Deserialize<'de> + AsRef<[u8]> + Clone,
{
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de, BufType> Visitor<'de> for NodeSeed<'_, BufType>
where
    BufType: Deserialize<'de> + AsRef<[u8]> + Clone,
{
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a BEP 52 file tree dictionary")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut file: Option<FileProps<BufType>> = None;
        let mut has_children = false;
        while let Some(key) = map.next_key::<BufType>()? {
            if key.as_ref().is_empty() {
                // "" holds the properties of the file at this path. Keys are
                // sorted, so it comes before any child.
                file = Some(map.next_value()?);
                continue;
            }
            has_children = true;
            if self.prefix.len() >= self.limits.depth {
                // Stop descending: the bencode decoder already bounds the
                // recursion, this bounds the work and the path length.
                self.tree.problem = Some(FileTreeProblem::TooDeep);
                map.next_value::<IgnoredAny>()?;
                continue;
            }
            self.prefix.push(key);
            let res = map.next_value_seed(NodeSeed {
                prefix: &mut *self.prefix,
                tree: &mut *self.tree,
                path_components: &mut *self.path_components,
                limits: self.limits,
            });
            self.prefix.pop();
            res?;
        }
        let Some(file) = file else {
            return Ok(());
        };
        if self.prefix.is_empty() {
            self.tree.problem = Some(FileTreeProblem::RootIsFile);
            return Ok(());
        }
        if has_children {
            self.tree.problem = Some(FileTreeProblem::MixedNode);
            return Ok(());
        }
        if self.tree.files.len() >= self.limits.files {
            self.tree.problem = Some(FileTreeProblem::TooManyFiles);
            return Ok(());
        }
        *self.path_components = self.path_components.saturating_add(self.prefix.len());
        if *self.path_components > self.limits.path_components {
            self.tree.problem = Some(FileTreeProblem::TooManyPathComponents);
            return Ok(());
        }
        self.tree.files.push(V2FileTreeEntry {
            path: self.prefix.clone(),
            length: file.length,
            pieces_root: file.pieces_root,
            attr: file.attr,
        });
        Ok(())
    }
}

impl<BufType> V2FileTree<BufType> {
    fn deserialize_limited<'de, D>(deserializer: D, limits: TreeLimits) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
        BufType: Deserialize<'de> + AsRef<[u8]> + Clone,
    {
        let mut tree = V2FileTree::default();
        let mut prefix = Vec::new();
        let mut path_components = 0;
        NodeSeed {
            prefix: &mut prefix,
            tree: &mut tree,
            path_components: &mut path_components,
            limits,
        }
        .deserialize(deserializer)?;
        Ok(tree)
    }
}

impl<'de, BufType> Deserialize<'de> for V2FileTree<BufType>
where
    BufType: Deserialize<'de> + AsRef<[u8]> + Clone,
{
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::deserialize_limited(
            deserializer,
            TreeLimits {
                depth: MAX_FILE_TREE_DEPTH,
                files: MAX_FILE_TREE_FILES,
                path_components: MAX_FILE_TREE_PATH_COMPONENTS,
            },
        )
    }
}

/// The files sharing the first `depth` path components, serialized as the
/// nested dictionary below that point.
struct TreeLevel<'a, BufType> {
    files: &'a [V2FileTreeEntry<BufType>],
    depth: usize,
}

enum TreeNode<'a, BufType> {
    File(&'a V2FileTreeEntry<BufType>),
    Dir(TreeLevel<'a, BufType>),
}

struct FileLeaf<'a, BufType>(&'a V2FileTreeEntry<BufType>);

impl<BufType: Serialize> Serialize for FileLeaf<'_, BufType> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry(
            &buffers::ByteBuf(b""),
            &FilePropsRef {
                attr: self.0.attr.as_ref(),
                length: self.0.length,
                pieces_root: self.0.pieces_root.as_ref(),
            },
        )?;
        map.end()
    }
}

impl<BufType: Serialize + AsRef<[u8]>> Serialize for TreeNode<'_, BufType> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            TreeNode::File(f) => FileLeaf(*f).serialize(serializer),
            TreeNode::Dir(level) => level.serialize(serializer),
        }
    }
}

impl<BufType: Serialize + AsRef<[u8]>> Serialize for TreeLevel<'_, BufType> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        let mut rest = self.files;
        while let Some(first) = rest.first() {
            let Some(key) = first.path.get(self.depth) else {
                return Err(serde::ser::Error::custom("file tree path too short"));
            };
            let group_len = rest
                .iter()
                .take_while(|f| f.path.get(self.depth).map(|k| k.as_ref()) == Some(key.as_ref()))
                .count();
            let (group, tail) = rest.split_at(group_len);
            rest = tail;
            let node = match group {
                [file] if file.path.len() == self.depth + 1 => TreeNode::File(file),
                _ => TreeNode::Dir(TreeLevel {
                    files: group,
                    depth: self.depth + 1,
                }),
            };
            map.serialize_entry(key, &node)?;
        }
        map.end()
    }
}

impl<BufType: Serialize + AsRef<[u8]>> Serialize for V2FileTree<BufType> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        TreeLevel {
            files: &self.files,
            depth: 0,
        }
        .serialize(serializer)
    }
}

impl<BufType: CloneToOwned> CloneToOwned for V2FileTreeEntry<BufType> {
    type Target = V2FileTreeEntry<<BufType as CloneToOwned>::Target>;

    fn clone_to_owned(&self, within_buffer: Option<&Bytes>) -> Self::Target {
        V2FileTreeEntry {
            path: self.path.clone_to_owned(within_buffer),
            length: self.length,
            pieces_root: self.pieces_root.clone_to_owned(within_buffer),
            attr: self.attr.clone_to_owned(within_buffer),
        }
    }
}

impl<BufType: CloneToOwned> CloneToOwned for V2FileTree<BufType> {
    type Target = V2FileTree<<BufType as CloneToOwned>::Target>;

    fn clone_to_owned(&self, within_buffer: Option<&Bytes>) -> Self::Target {
        V2FileTree {
            files: self.files.clone_to_owned(within_buffer),
            problem: self.problem,
        }
    }
}

/// The `piece layers` dictionary of a v2 torrent file: pieces root -> the
/// concatenated hashes of that file's piece layer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PieceLayers<BufType>(pub Vec<(BufType, BufType)>);

impl<BufType: AsRef<[u8]>> PieceLayers<BufType> {
    /// The raw layer of the file with this pieces root.
    pub fn get(&self, pieces_root: &Id32) -> Option<&BufType> {
        self.0
            .iter()
            .find(|(k, _)| k.as_ref() == pieces_root.0)
            .map(|(_, v)| v)
    }
}

impl<'de, BufType: Deserialize<'de>> Deserialize<'de> for PieceLayers<BufType> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V<B>(PhantomData<B>);
        impl<'de, B: Deserialize<'de>> Visitor<'de> for V<B> {
            type Value = PieceLayers<B>;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a BEP 52 piece layers dictionary")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                // Every entry is at least 32 + 32 bytes of input, so the
                // input size bounds this vector.
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry::<B, B>()? {
                    entries.push(entry);
                }
                Ok(PieceLayers(entries))
            }
        }
        deserializer.deserialize_map(V(PhantomData))
    }
}

impl<BufType: Serialize> Serialize for PieceLayers<BufType> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

impl<BufType: CloneToOwned> CloneToOwned for PieceLayers<BufType> {
    type Target = PieceLayers<<BufType as CloneToOwned>::Target>;

    fn clone_to_owned(&self, within_buffer: Option<&Bytes>) -> Self::Target {
        PieceLayers(
            self.0
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone_to_owned(within_buffer),
                        v.clone_to_owned(within_buffer),
                    )
                })
                .collect(),
        )
    }
}

/// Where one non-empty file of a v2 or hybrid torrent sits in piece space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct V2FileInfo {
    /// Index of the file among the files of the torrent as iterated by
    /// `iter_file_details` (for hybrid torrents that list includes padding
    /// files).
    pub file_index: usize,
    /// Length of the file in bytes.
    pub length: u64,
    /// Root of the file's merkle tree.
    pub pieces_root: Id32,
    /// The file's first piece.
    pub first_piece: u32,
    /// How many pieces the file covers.
    pub num_pieces: u32,
}

impl V2FileInfo {
    /// True if the file has a `piece layers` entry, i.e. is larger than one
    /// piece. Smaller files are verified against their pieces root alone.
    pub fn has_piece_layer(&self, piece_length: u32) -> bool {
        self.length > u64::from(piece_length)
    }
}

/// v2 hashing data of a validated torrent: its non-empty files, and their
/// piece layers. Layers can be learned after the torrent was added (from
/// peers, for magnets), so each one is set at most once, through a shared
/// reference.
#[derive(Debug)]
pub(crate) struct V2Hashes {
    pub(crate) files: Box<[V2FileInfo]>,
    // One per entry of files. Stays empty for files without a piece layer.
    pub(crate) layers: Box<[OnceLock<Box<[Id32]>>]>,
    // (pieces root, position in files), sorted, for lookups by root.
    pub(crate) by_root: Box<[(Id32, u32)]>,
}

impl V2Hashes {
    pub(crate) fn new(files: Vec<V2FileInfo>) -> Arc<Self> {
        let layers = files.iter().map(|_| OnceLock::new()).collect();
        // Validation bounds the file count far below u32::MAX.
        let mut by_root: Vec<(Id32, u32)> = files
            .iter()
            .zip(0u32..)
            .map(|(f, idx)| (f.pieces_root, idx))
            .collect();
        by_root.sort_unstable();
        Arc::new(Self {
            files: files.into_boxed_slice(),
            layers,
            by_root: by_root.into_boxed_slice(),
        })
    }

    /// The position in `files` of the file that covers `piece`.
    pub(crate) fn file_for_piece(&self, piece: u32) -> Option<usize> {
        // Files are in piece order and their piece ranges do not overlap.
        let pos = self.files.partition_point(|f| {
            u64::from(f.first_piece) + u64::from(f.num_pieces) <= u64::from(piece)
        });
        let f = self.files.get(pos)?;
        (f.first_piece <= piece).then_some(pos)
    }
}

/// Verifies one piece of a torrent from its bytes, whatever the torrent's
/// version: the SHA-1 of the whole piece for v1, the merkle hash of the
/// file's bytes in the piece for v2, both for hybrid torrents.
///
/// Feed the piece's bytes in order with [`PieceVerifier::update`] (for hybrid
/// torrents that includes the bytes of padding files, read as zeros), then
/// call [`PieceVerifier::finish`].
#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
pub struct PieceVerifier {
    v1: Option<(sha1w::Sha1, [u8; 20])>,
    v2: Option<V2PieceCheck>,
    piece_len: u64,
    fed: u64,
}

#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
struct V2PieceCheck {
    hasher: crate::merkle::BlockTreeHasher,
    leaves: u64,
    expected: Id32,
    // How many of the piece's bytes belong to the file (the rest, in hybrid
    // torrents, is padding and not part of the v2 tree).
    file_bytes: u64,
}

#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
impl PieceVerifier {
    pub(crate) fn new(piece_len: u32) -> Self {
        Self {
            v1: None,
            v2: None,
            piece_len: u64::from(piece_len),
            fed: 0,
        }
    }

    pub(crate) fn with_v1(mut self, expected: [u8; 20]) -> Self {
        use sha1w::ISha1;
        self.v1 = Some((sha1w::Sha1::new(), expected));
        self
    }

    pub(crate) fn with_v2(mut self, expected: Id32, leaves: u64, file_bytes: u64) -> Self {
        self.v2 = Some(V2PieceCheck {
            hasher: crate::merkle::BlockTreeHasher::new(),
            leaves,
            expected,
            file_bytes,
        });
        self
    }

    /// True if this verifier checks the v1 SHA-1 hash.
    pub fn checks_v1(&self) -> bool {
        self.v1.is_some()
    }

    /// True if this verifier checks the v2 merkle hash.
    pub fn checks_v2(&self) -> bool {
        self.v2.is_some()
    }

    /// Feed the next bytes of the piece.
    pub fn update(&mut self, data: &[u8]) {
        if let Some((h, _)) = self.v1.as_mut() {
            use sha1w::ISha1;
            h.update(data);
        }
        if let Some(v2) = self.v2.as_mut() {
            let left = v2.file_bytes.saturating_sub(self.fed);
            let take = usize::try_from(left).unwrap_or(usize::MAX).min(data.len());
            if let Some(file_part) = data.get(..take) {
                v2.hasher.update(file_part);
            }
        }
        self.fed = self
            .fed
            .saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
    }

    /// True if exactly the piece's length was fed and every hash matches.
    pub fn finish(self) -> bool {
        if self.fed != self.piece_len {
            return false;
        }
        if let Some((h, expected)) = self.v1 {
            use sha1w::ISha1;
            if h.finish() != expected {
                return false;
            }
        }
        if let Some(v2) = self.v2
            && v2.hasher.finish(v2.leaves) != Some(v2.expected)
        {
            return false;
        }
        true
    }
}
