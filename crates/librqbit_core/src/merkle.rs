//! Merkle hash trees of BitTorrent v2 (BEP 52).
//!
//! Every file of a v2 torrent has its own binary SHA-256 tree. The leaves are
//! the hashes of the file's 16 KiB blocks (the last block hashed at its real
//! length). The leaf count is rounded up to a power of two with leaves of 32
//! zero bytes, and every inner node is `SHA-256(left || right)`. The root of
//! that tree is the file's `pieces root`; the layer whose nodes each cover one
//! piece is the file's "piece layer".
//!
//! Heights below are counted from the leaf (block) layer: height 0 is the
//! block layer, height `log2(piece_length / 16 KiB)` is the piece layer.
//!
//! The geometry helpers are always available; everything that hashes needs
//! one of the `sha1-*` features (which also provide SHA-256).

#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
use sha1w::ISha256;

#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
use crate::hash_id::Id32;

/// Size of a leaf block of the merkle tree.
pub const BLOCK_SIZE: u32 = 16 * 1024;

/// Upper bound on the height of any tree we build: 2^64 leaves.
#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
const MAX_HEIGHT: usize = 64;

/// SHA-256 of one block (the leaf hash).
#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
pub fn hash_block(data: &[u8]) -> Id32 {
    let mut h = sha1w::Sha256::new();
    h.update(data);
    Id32::new(h.finish())
}

/// The parent of two nodes: `SHA-256(left || right)`.
#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
pub fn hash_pair(left: &Id32, right: &Id32) -> Id32 {
    let mut h = sha1w::Sha256::new();
    h.update(&left.0);
    h.update(&right.0);
    Id32::new(h.finish())
}

/// The root of a subtree of `2^height` padding leaves (each 32 zero bytes).
///
/// `pad_hash(0)` is the padding leaf itself; `pad_hash(height_of_piece_layer)`
/// is what fills a piece layer up to a power of two.
#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
pub fn pad_hash(height: u32) -> Id32 {
    let mut h = Id32::default();
    for _ in 0..height {
        h = hash_pair(&h, &h);
    }
    h
}

/// Number of 16 KiB blocks of a file of `file_len` bytes.
pub fn num_blocks(file_len: u64) -> u64 {
    file_len.div_ceil(BLOCK_SIZE as u64)
}

/// Number of leaves of the tree of a file: its block count rounded up to a
/// power of two (at least one).
pub fn num_leafs(file_len: u64) -> Option<u64> {
    num_blocks(file_len).max(1).checked_next_power_of_two()
}

/// Height of the piece layer for the given piece length, or None if the piece
/// length is not a power of two of at least 16 KiB.
pub fn piece_layer_height(piece_length: u32) -> Option<u32> {
    if piece_length < BLOCK_SIZE || !piece_length.is_power_of_two() {
        return None;
    }
    Some((piece_length / BLOCK_SIZE).trailing_zeros())
}

/// Builds the root of a tree from its leaves in order without storing them.
///
/// Works like a binary counter: `nodes[h]` holds a finished left subtree of
/// height `h` exactly when bit `h` of the leaf count is set, so memory is a
/// fixed 2 KiB array regardless of the number of leaves.
#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
#[derive(Clone)]
pub struct MerkleRootBuilder {
    nodes: [Id32; MAX_HEIGHT],
    leaves: u64,
    pad_leaf: Id32,
}

#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
impl MerkleRootBuilder {
    /// A builder whose missing leaves are filled with `pad_leaf`. Use
    /// `Id32::default()` (zeros) for block trees and `pad_hash(h)` for trees
    /// built from layer `h`.
    pub fn new(pad_leaf: Id32) -> Self {
        Self {
            nodes: [Id32::default(); MAX_HEIGHT],
            leaves: 0,
            pad_leaf,
        }
    }

    /// How many leaves were pushed so far.
    pub fn leaves(&self) -> u64 {
        self.leaves
    }

    /// Append the next leaf.
    pub fn push(&mut self, leaf: Id32) {
        let mut carry = leaf;
        let mut height = 0;
        while height < MAX_HEIGHT && self.leaves & (1 << height) != 0 {
            carry = hash_pair(&self.nodes[height], &carry);
            height += 1;
        }
        if height < MAX_HEIGHT {
            self.nodes[height] = carry;
        }
        self.leaves = self.leaves.wrapping_add(1);
    }

    /// The root of the tree with `leaf_count` leaves (a power of two), the
    /// leaves not pushed being padding. None if `leaf_count` is not a power of
    /// two or fewer leaves than were pushed.
    pub fn finish(&self, leaf_count: u64) -> Option<Id32> {
        if !leaf_count.is_power_of_two() || self.leaves > leaf_count {
            return None;
        }
        let target = leaf_count.trailing_zeros() as usize;
        if self.leaves == leaf_count {
            return Some(self.nodes[target]);
        }
        let mut pad = self.pad_leaf;
        let mut pad_height = 0;
        let mut carry: Option<Id32> = None;
        for height in 0..target {
            let slot = (self.leaves & (1 << height) != 0).then_some(&self.nodes[height]);
            if slot.is_none() && carry.is_none() {
                continue;
            }
            while pad_height < height {
                pad = hash_pair(&pad, &pad);
                pad_height += 1;
            }
            carry = Some(match (slot, carry) {
                (Some(left), Some(right)) => hash_pair(left, &right),
                (Some(left), None) => hash_pair(left, &pad),
                (None, Some(left)) => hash_pair(&left, &pad),
                (None, None) => continue,
            });
        }
        match carry {
            Some(root) => Some(root),
            // Nothing was pushed: the whole tree is padding.
            None => {
                while pad_height < target {
                    pad = hash_pair(&pad, &pad);
                    pad_height += 1;
                }
                Some(pad)
            }
        }
    }
}

/// Streams bytes, hashes them in 16 KiB blocks and builds the root over the
/// block hashes. The last block may be short; it is hashed at its real length.
#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
pub struct BlockTreeHasher {
    builder: MerkleRootBuilder,
    current: sha1w::Sha256,
    in_block: u32,
}

#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
impl Default for BlockTreeHasher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
impl BlockTreeHasher {
    /// An empty hasher.
    pub fn new() -> Self {
        Self {
            builder: MerkleRootBuilder::new(Id32::default()),
            current: sha1w::Sha256::new(),
            in_block: 0,
        }
    }

    /// Feed the next bytes.
    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            let take = ((BLOCK_SIZE - self.in_block) as usize).min(data.len());
            let (head, tail) = data.split_at(take);
            self.current.update(head);
            // take <= BLOCK_SIZE, fits u32.
            self.in_block += take as u32;
            data = tail;
            if self.in_block == BLOCK_SIZE {
                self.flush_block();
            }
        }
    }

    fn flush_block(&mut self) {
        let h = std::mem::replace(&mut self.current, sha1w::Sha256::new());
        self.builder.push(Id32::new(h.finish()));
        self.in_block = 0;
    }

    /// The root over `leaf_count` leaves (a power of two). None if more blocks
    /// were fed than `leaf_count`.
    pub fn finish(mut self, leaf_count: u64) -> Option<Id32> {
        if self.in_block > 0 {
            self.flush_block();
        }
        self.builder.finish(leaf_count)
    }
}

/// The `pieces root` of a file from its piece layer. None if the layer is
/// empty or the piece length is invalid.
#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
pub fn pieces_root_from_piece_layer(piece_layer: &[Id32], piece_length: u32) -> Option<Id32> {
    let height = piece_layer_height(piece_length)?;
    if piece_layer.is_empty() {
        return None;
    }
    let width = u64::try_from(piece_layer.len())
        .ok()?
        .checked_next_power_of_two()?;
    let mut b = MerkleRootBuilder::new(pad_hash(height));
    for h in piece_layer {
        b.push(*h);
    }
    b.finish(width)
}

/// The `pieces root` of a whole file computed from its content.
#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
pub fn pieces_root_from_data(data: &[u8]) -> Option<Id32> {
    if data.is_empty() {
        return None;
    }
    let mut h = BlockTreeHasher::new();
    h.update(data);
    h.finish(num_leafs(data.len() as u64)?)
}

/// The piece layer of a whole file computed from its content: one hash per
/// piece, each the root of a subtree of `piece_length / 16 KiB` leaves.
#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
pub fn piece_layer_from_data(data: &[u8], piece_length: u32) -> Option<Vec<Id32>> {
    let height = piece_layer_height(piece_length)?;
    let leaves = 1u64 << height;
    data.chunks(piece_length as usize)
        .map(|piece| {
            let mut h = BlockTreeHasher::new();
            h.update(piece);
            h.finish(leaves)
        })
        .collect()
}

/// Number of uncle hashes in a BEP 52 `hashes` message: one per proof layer,
/// minus the `log2(count) - 1` lowest proof layers which the requester
/// computes from the base hashes itself.
pub fn num_uncle_hashes(count: u32, proof_layers: u32) -> u32 {
    let omitted = count.max(1).trailing_zeros().saturating_sub(1);
    proof_layers.saturating_sub(omitted)
}

/// Upper bound we accept for the `length` of a hash request, both ways. BEP
/// 52 says requests SHOULD NOT exceed 512 hashes; libtorrent asks for the
/// block hashes of a whole piece at once, so it accepts up to 8192.
pub const MAX_HASH_REQUEST_COUNT: u32 = 8192;

/// Geometry of a BEP 52 hash request against the tree of one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashRequestShape {
    /// Layer of the requested hashes, counted from the block layer.
    pub base: u32,
    /// Index of the first requested hash in the base layer.
    pub index: u32,
    /// Number of requested hashes; a power of two, at least two.
    pub count: u32,
    /// Number of ancestor layers to prove.
    pub proof_layers: u32,
}

impl HashRequestShape {
    /// Checks the request against the tree of a file of `file_len` bytes, with
    /// the rules of BEP 52 and the same bounds as libtorrent: `count` a power
    /// of two in 2..=8192, `index` a multiple of `count`, the requested range
    /// inside the layer, the proof not reaching above the root.
    pub fn is_valid_for(&self, file_len: u64) -> bool {
        let Some(leafs) = num_leafs(file_len) else {
            return false;
        };
        if file_len == 0 {
            return false;
        }
        let tree_height = leafs.trailing_zeros();
        if self.count < 2
            || self.count > MAX_HASH_REQUEST_COUNT
            || !self.count.is_power_of_two()
            || !self.index.is_multiple_of(self.count)
            || self.base >= tree_height
        {
            return false;
        }
        let layer_width = leafs >> self.base;
        let end = u64::from(self.index) + u64::from(self.count);
        if end > layer_width {
            return false;
        }
        self.proof_layers < tree_height - self.base
    }

    /// Total number of hashes in the matching `hashes` message.
    pub fn response_len(&self) -> u32 {
        self.count
            .saturating_add(num_uncle_hashes(self.count, self.proof_layers))
    }
}

/// Root of a full layer of `hashes` (a power-of-two count, no padding).
#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
fn subtree_root(hashes: &[Id32]) -> Option<Id32> {
    let count = u64::try_from(hashes.len()).ok()?;
    let mut b = MerkleRootBuilder::new(Id32::default());
    for h in hashes {
        b.push(*h);
    }
    b.finish(count)
}

/// Verifies the payload of a BEP 52 `hashes` message: the `shape.count` base
/// hashes followed by the uncle hashes, bottom up. True if, hashed up through
/// the proof, they produce `pieces_root` of a file of `file_len` bytes. The
/// shape must request a proof all the way up to the root.
#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
pub fn verify_hashes(
    pieces_root: &Id32,
    file_len: u64,
    shape: &HashRequestShape,
    hashes: &[Id32],
) -> bool {
    if !shape.is_valid_for(file_len) {
        return false;
    }
    let Some(tree_height) = num_leafs(file_len).map(|l| l.trailing_zeros()) else {
        return false;
    };
    // Without the full proof the hashes cannot be tied to the root.
    if shape.proof_layers + 1 != tree_height - shape.base {
        return false;
    }
    let count = shape.count as usize;
    if hashes.len() != shape.response_len() as usize {
        return false;
    }
    let (base, uncles) = hashes.split_at(count);
    let Some(mut node) = subtree_root(base) else {
        return false;
    };
    let mut index = u64::from(shape.index / shape.count);
    for uncle in uncles {
        node = if index & 1 == 0 {
            hash_pair(&node, uncle)
        } else {
            hash_pair(uncle, &node)
        };
        index >>= 1;
    }
    index == 0 && node == *pieces_root
}

/// The tree of one file from its piece layer up to its root, kept to answer
/// BEP 52 hash requests for the piece layer and the layers above it.
#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
#[derive(Debug, Clone)]
pub struct PieceLayerTree {
    piece_height: u32,
    // layers[0] is the piece layer padded to a power of two, the last layer
    // holds just the root.
    layers: Vec<Box<[Id32]>>,
}

#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
impl PieceLayerTree {
    /// Build from a file's piece layer. None for an empty layer or an invalid
    /// piece length.
    pub fn new(piece_layer: &[Id32], piece_length: u32) -> Option<Self> {
        let piece_height = piece_layer_height(piece_length)?;
        if piece_layer.is_empty() {
            return None;
        }
        let width = piece_layer.len().checked_next_power_of_two()?;
        let pad = pad_hash(piece_height);
        let mut bottom = Vec::with_capacity(width);
        bottom.extend_from_slice(piece_layer);
        bottom.resize(width, pad);
        let mut layers: Vec<Box<[Id32]>> = vec![bottom.into_boxed_slice()];
        while let Some(last) = layers.last().filter(|l| l.len() > 1) {
            let up: Box<[Id32]> = last
                .as_chunks::<2>()
                .0
                .iter()
                .map(|[left, right]| hash_pair(left, right))
                .collect();
            layers.push(up);
        }
        Some(Self {
            piece_height,
            layers,
        })
    }

    /// The `pieces root` this tree hashes to.
    pub fn root(&self) -> Id32 {
        self.layers
            .last()
            .and_then(|l| l.first())
            .copied()
            .unwrap_or_default()
    }

    /// The payload of the `hashes` message answering `shape`, or None if the
    /// request asks for something below the piece layer or out of range.
    pub fn hashes_for(&self, shape: &HashRequestShape) -> Option<Vec<Id32>> {
        if !shape.count.is_power_of_two() {
            return None;
        }
        let rel = shape.base.checked_sub(self.piece_height)? as usize;
        let layer = self.layers.get(rel)?;
        let start = shape.index as usize;
        let end = start.checked_add(shape.count as usize)?;
        let base = layer.get(start..end)?;
        let uncles = num_uncle_hashes(shape.count, shape.proof_layers) as usize;
        let mut out = Vec::with_capacity(base.len() + uncles);
        out.extend_from_slice(base);
        // The first uncle is the sibling of the subtree root of the base
        // hashes, which sits log2(count) layers above the base.
        let first_uncle_layer = rel + shape.count.trailing_zeros() as usize;
        let mut index = start >> shape.count.trailing_zeros();
        for l in first_uncle_layer..first_uncle_layer + uncles {
            let layer = self.layers.get(l)?;
            out.push(*layer.get(index ^ 1)?);
            index >>= 1;
        }
        Some(out)
    }
}

#[cfg(all(test, any(feature = "sha1-ring", feature = "sha1-crypto-hash")))]
mod tests {
    use super::*;

    fn naive_root(mut layer: Vec<Id32>, width: usize, pad: Id32) -> Id32 {
        layer.resize(width, pad);
        while layer.len() > 1 {
            layer = layer
                .as_chunks::<2>()
                .0
                .iter()
                .map(|[left, right]| hash_pair(left, right))
                .collect();
        }
        layer[0]
    }

    fn leaf(i: u8) -> Id32 {
        hash_block(&[i; 7])
    }

    #[test]
    fn test_builder_matches_naive_for_all_small_sizes() {
        for n in 1..=33usize {
            let leaves: Vec<Id32> = (0..n).map(|i| leaf(i as u8)).collect();
            let width = n.next_power_of_two();
            for extra in [1usize, 2, 4] {
                let w = width * extra;
                let mut b = MerkleRootBuilder::new(Id32::default());
                for l in &leaves {
                    b.push(*l);
                }
                assert_eq!(
                    b.finish(w as u64).unwrap(),
                    naive_root(leaves.clone(), w, Id32::default()),
                    "n={n} w={w}"
                );
            }
        }
    }

    #[test]
    fn test_builder_rejects_bad_counts() {
        let mut b = MerkleRootBuilder::new(Id32::default());
        b.push(leaf(1));
        b.push(leaf(2));
        b.push(leaf(3));
        assert!(b.finish(2).is_none());
        assert!(b.finish(3).is_none());
        assert!(b.finish(4).is_some());
    }

    #[test]
    fn test_empty_builder_is_padding() {
        let b = MerkleRootBuilder::new(Id32::default());
        assert_eq!(b.finish(8).unwrap(), pad_hash(3));
        let b = MerkleRootBuilder::new(pad_hash(2));
        assert_eq!(b.finish(4).unwrap(), pad_hash(4));
    }

    #[test]
    fn test_block_hasher_chunking_is_irrelevant() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i * 7 + i / 300) as u8).collect();
        let expected = {
            let leaves: Vec<Id32> = data.chunks(BLOCK_SIZE as usize).map(hash_block).collect();
            naive_root(leaves, 8, Id32::default())
        };
        for step in [1usize, 1000, 16384, 16385, 100_000] {
            let mut h = BlockTreeHasher::new();
            for c in data.chunks(step) {
                h.update(c);
            }
            assert_eq!(h.finish(8).unwrap(), expected, "step={step}");
        }
        assert_eq!(pieces_root_from_data(&data).unwrap(), expected);
    }

    #[test]
    fn test_piece_layer_root_equals_file_root() {
        // 5 pieces of 32 KiB, the last one short: the root over the piece
        // layer padded with pad_hash(1) must equal the root over all blocks.
        let piece_length = 32768;
        for len in [32769usize, 65536, 100_000, 5 * 32768 - 1, 5 * 32768] {
            let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let layer = piece_layer_from_data(&data, piece_length).unwrap();
            assert_eq!(
                pieces_root_from_piece_layer(&layer, piece_length),
                pieces_root_from_data(&data),
                "len={len}"
            );
        }
    }

    #[test]
    fn test_pieces_root_rejects_invalid_piece_length() {
        assert!(pieces_root_from_piece_layer(&[leaf(1)], 1000).is_none());
        assert!(pieces_root_from_piece_layer(&[leaf(1)], 8192).is_none());
        assert!(pieces_root_from_piece_layer(&[], 16384).is_none());
    }

    #[test]
    fn test_hash_request_shape_validation() {
        // 1 MiB file: 64 blocks, tree height 6.
        let len = 1 << 20;
        let ok = HashRequestShape {
            base: 0,
            index: 0,
            count: 64,
            proof_layers: 0,
        };
        assert!(ok.is_valid_for(len));
        // Count not a power of two, too small, index not aligned, out of range.
        for bad in [
            HashRequestShape { count: 3, ..ok },
            HashRequestShape { count: 1, ..ok },
            HashRequestShape {
                index: 2,
                count: 4,
                ..ok
            },
            HashRequestShape {
                index: 64,
                count: 64,
                ..ok
            },
            HashRequestShape { count: 128, ..ok },
            HashRequestShape { base: 6, ..ok },
            HashRequestShape {
                count: 2,
                proof_layers: 6,
                ..ok
            },
        ] {
            assert!(!bad.is_valid_for(len), "{bad:?}");
        }
        assert!(!ok.is_valid_for(0));
    }

    #[test]
    fn test_hashes_roundtrip_and_forgeries() {
        // 10 pieces of 16 KiB each: the piece layer is the block layer.
        for (piece_length, len) in [(16384u32, 10 * 16384u64 - 5), (65536, 37 * 65536)] {
            let data: Vec<u8> = (0..len).map(|i| (i % 253) as u8).collect();
            let layer = piece_layer_from_data(&data, piece_length).unwrap();
            let root = pieces_root_from_data(&data).unwrap();
            let tree = PieceLayerTree::new(&layer, piece_length).unwrap();
            assert_eq!(tree.root(), root);

            let base = piece_layer_height(piece_length).unwrap();
            let tree_height = num_leafs(len).unwrap().trailing_zeros();
            let width = 1u32 << (tree_height - base);
            for count in [2u32, 4, 8, 16, 64] {
                if count > width {
                    continue;
                }
                for index in (0..width).step_by(count as usize) {
                    let shape = HashRequestShape {
                        base,
                        index,
                        count,
                        proof_layers: tree_height - base - 1,
                    };
                    let hashes = tree.hashes_for(&shape).unwrap();
                    assert_eq!(hashes.len(), shape.response_len() as usize);
                    assert!(verify_hashes(&root, len, &shape, &hashes), "{shape:?}");

                    // Flip any single hash: the proof must fail.
                    for i in 0..hashes.len() {
                        let mut forged = hashes.clone();
                        forged[i].0[0] ^= 1;
                        assert!(!verify_hashes(&root, len, &shape, &forged));
                    }
                    // Wrong lengths fail.
                    assert!(!verify_hashes(&root, len, &shape, &hashes[1..]));
                    let mut longer = hashes.clone();
                    longer.push(Id32::default());
                    assert!(!verify_hashes(&root, len, &shape, &longer));
                }
            }
        }
    }

    #[test]
    fn test_hashes_for_rejects_below_piece_layer() {
        let data = vec![1u8; 4 * 65536];
        let layer = piece_layer_from_data(&data, 65536).unwrap();
        let tree = PieceLayerTree::new(&layer, 65536).unwrap();
        let shape = HashRequestShape {
            base: 0,
            index: 0,
            count: 4,
            proof_layers: 0,
        };
        assert!(tree.hashes_for(&shape).is_none());
    }

    #[test]
    fn test_num_uncle_hashes() {
        assert_eq!(num_uncle_hashes(2, 3), 3);
        assert_eq!(num_uncle_hashes(4, 3), 2);
        assert_eq!(num_uncle_hashes(512, 9), 1);
        assert_eq!(num_uncle_hashes(512, 7), 0);
    }
}
