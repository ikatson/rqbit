//! SHA-256 merkle tree verification for BEP 52 (BitTorrent v2).
//!
//! BEP 52 uses a binary merkle tree of SHA-256 hashes over 16 KiB blocks.
//! Piece hashes in `piece layers` are the roots of per-piece subtrees.
//! The file's `pieces_root` is the root of the full merkle tree over all pieces.
//!
//! # Performance Characteristics
//!
//! - `verify_piece`: O(blocks_per_piece) time, O(blocks_per_piece) space
//! - `compute_merkle_root`: O(blocks) time, O(blocks) space
//! - `root_from_piece_layer`: O(pieces) time, O(pieces) space
//!
//! Typical piece_length (64 KiB) with MERKLE_BLOCK_SIZE (16 KiB):
//! - blocks_per_piece = 4
//! - Tree depth = 2
//! - Memory per piece verification: ~128 bytes (4 * 32-byte hashes)

use crate::Id32;

/// Fixed 16 KiB block size used for merkle leaf hashing in BEP 52.
pub const MERKLE_BLOCK_SIZE: u32 = 16384;

/// BEP 52: padding leaf hashes beyond EOF are all-zero bytes (NOT SHA-256 of zeros).
pub fn zero_hash() -> Id32 {
    Id32::new([0u8; 32])
}

/// SHA-256 hash of a single data block.
pub fn hash_block(data: &[u8]) -> Id32 {
    use sha1w::ISha256;
    let mut h = sha1w::Sha256::new();
    h.update(data);
    Id32::new(h.finish())
}

/// SHA-256(left || right) — internal merkle node hash.
pub fn hash_pair(left: &Id32, right: &Id32) -> Id32 {
    use sha1w::ISha256;
    let mut h = sha1w::Sha256::new();
    h.update(&left.0);
    h.update(&right.0);
    Id32::new(h.finish())
}

/// Verify a piece's block hashes against the expected piece hash from `piece layers`.
///
/// `block_hashes`: SHA-256 hashes of each 16 KiB block in this piece.
/// `expected`: the expected piece-layer hash (root of this piece's merkle subtree).
/// `blocks_per_piece`: `piece_length / MERKLE_BLOCK_SIZE` — always a power of 2 per BEP 52.
///
/// Pads `block_hashes` to `blocks_per_piece` with `zero_hash()`, builds the
/// subtree bottom-up, and compares the root against `expected`.
pub fn verify_piece(block_hashes: &[Id32], expected: &Id32, blocks_per_piece: u32) -> bool {
    let bpp = blocks_per_piece as usize;
    debug_assert!(bpp.is_power_of_two());
    if block_hashes.len() > bpp {
        return false;
    }

    if bpp <= 8 {
        let mut layer = [zero_hash(); 8];
        layer[..block_hashes.len()].copy_from_slice(block_hashes);
        for slot in layer.iter_mut().take(bpp).skip(block_hashes.len()) {
            *slot = zero_hash();
        }
        let mut size = bpp;
        while size > 1 {
            for i in 0..(size / 2) {
                layer[i] = hash_pair(&layer[i * 2], &layer[i * 2 + 1]);
            }
            size /= 2;
        }
        return layer[0] == *expected;
    }

    // Build the leaf layer, padding with zeros.
    let mut layer: Vec<Id32> = Vec::with_capacity(bpp);
    layer.extend_from_slice(block_hashes);
    layer.resize(bpp, zero_hash());

    // Build tree bottom-up.
    while layer.len() > 1 {
        let mut next = Vec::with_capacity(layer.len() / 2);
        for pair in layer.chunks_exact(2) {
            next.push(hash_pair(&pair[0], &pair[1]));
        }
        layer = next;
    }

    layer[0] == *expected
}

/// Verify a single block hash against a merkle proof path up to the piece root.
///
/// `block_hash`: SHA-256 hash of the block data.
/// `block_index_in_piece`: 0-based index of the block within the piece.
/// `proof`: sibling hashes from leaf level up to (but not including) the root.
/// `expected_piece_hash`: the expected piece-layer hash (root of this piece's subtree).
pub fn verify_block_with_proof(
    block_hash: &Id32,
    block_index_in_piece: u32,
    proof: &[Id32],
    expected_piece_hash: &Id32,
) -> bool {
    // Proof length defines the subtree depth. If index is out of range, fail.
    if (block_index_in_piece as usize) >= (1usize << proof.len()) {
        return false;
    }

    let mut idx = block_index_in_piece as usize;
    let mut hash = *block_hash;
    for sibling in proof {
        if idx & 1 == 0 {
            hash = hash_pair(&hash, sibling);
        } else {
            hash = hash_pair(sibling, &hash);
        }
        idx >>= 1;
    }

    hash == *expected_piece_hash
}

/// Result of computing a full merkle tree for a file.
pub struct MerkleResult {
    /// The merkle root (= pieces_root for the file tree entry).
    pub root: Id32,
    /// Piece-layer hashes. Only includes hashes for pieces that cover at least
    /// one byte of actual file data; trailing beyond-EOF pieces are omitted.
    /// For single-piece files, this is a single element equal to `root`.
    pub piece_hashes: Vec<Id32>,
}

/// Compute the full merkle tree for a file from its block hashes.
///
/// `block_hashes`: SHA-256 of each 16 KiB block in the file.
/// `blocks_per_piece`: `piece_length / MERKLE_BLOCK_SIZE` — must be a power of 2.
///
/// Returns the merkle root and the piece-layer hashes. The piece layer is the
/// tree layer where each node covers exactly `blocks_per_piece` leaves.
/// Only piece hashes covering at least one actual data block are included
/// (trailing beyond-EOF padding pieces are omitted from `piece_hashes`).
pub fn compute_merkle_root(block_hashes: &[Id32], blocks_per_piece: u32) -> MerkleResult {
    let bpp = blocks_per_piece as usize;
    debug_assert!(bpp.is_power_of_two() && bpp != 0);
    if bpp == 0 || !bpp.is_power_of_two() || block_hashes.is_empty() {
        // Zero-length files should not use merkle roots; return empty to avoid panics.
        return MerkleResult {
            root: zero_hash(),
            piece_hashes: Vec::new(),
        };
    }

    // Number of actual data pieces (ceil of block count / blocks_per_piece).
    let n_data_pieces = block_hashes.len().div_ceil(bpp);

    // Total leaf count: padded to next power of 2 at the full-tree level.
    // The tree has (n_data_pieces padded to power-of-2) * bpp leaves.
    let n_padded_pieces = n_data_pieces.next_power_of_two();
    let n_leaves = n_padded_pieces * bpp;

    let mut piece_layer: Option<Vec<Id32>> = None;
    let root = if n_leaves <= 64 {
        let mut layer = [zero_hash(); 64];
        layer[..block_hashes.len()].copy_from_slice(block_hashes);
        for slot in layer.iter_mut().take(n_leaves).skip(block_hashes.len()) {
            *slot = zero_hash();
        }
        let mut len = n_leaves;
        while len > 1 {
            let next_len = len / 2;
            for i in 0..next_len {
                layer[i] = hash_pair(&layer[i * 2], &layer[i * 2 + 1]);
            }
            if next_len == n_padded_pieces && piece_layer.is_none() {
                piece_layer = Some(layer[..n_data_pieces].to_vec());
            }
            len = next_len;
        }
        layer[0]
    } else {
        // Build the full leaf layer.
        let mut layer: Vec<Id32> = Vec::with_capacity(n_leaves);
        layer.extend_from_slice(block_hashes);
        layer.resize(n_leaves, zero_hash());

        // Build tree bottom-up, extracting the piece layer when we reach it.
        while layer.len() > 1 {
            let mut next = Vec::with_capacity(layer.len() / 2);
            for pair in layer.chunks_exact(2) {
                next.push(hash_pair(&pair[0], &pair[1]));
            }

            // The piece layer is where each node covers bpp leaves.
            // That happens when layer.len() == n_leaves / bpp == n_padded_pieces.
            // After one round of hashing, next.len() == layer.len() / 2.
            // We check if next.len() == n_padded_pieces.
            if next.len() == n_padded_pieces && piece_layer.is_none() {
                // Trim to only data pieces (omit trailing beyond-EOF padding).
                piece_layer = Some(next[..n_data_pieces].to_vec());
            }

            layer = next;
        }

        layer[0]
    };

    // If blocks_per_piece == 1, the piece layer IS the leaf layer (block hashes
    // padded to power-of-2), so we never entered the extraction branch above.
    // Similarly, if there's only 1 padded piece, the piece layer is extracted
    // when we first reduce to n_padded_pieces == 1, but that's also the root.
    let piece_hashes = piece_layer.unwrap_or_else(|| {
        if bpp == 1 {
            // Piece layer is the leaf layer; include only data pieces (no padding).
            return block_hashes.to_vec();
        }
        // bpp >= total leaves means single piece — piece hash is the root.
        vec![root]
    });

    MerkleResult { root, piece_hashes }
}

/// Compute the subtree root of `blocks_per_piece` all-zero leaves.
///
/// BEP 52 specifies that padding is at the block leaf level with zero hashes.
/// Internal nodes above zero leaves are computed normally via `hash_pair`.
/// This means the padding at the piece layer is NOT zero_hash() for bpp > 1.
fn padding_piece_hash(blocks_per_piece: u32) -> Id32 {
    let bpp = blocks_per_piece as usize;
    if bpp == 1 {
        return zero_hash();
    }
    if bpp <= 8 {
        let mut layer = [zero_hash(); 8];
        for slot in layer.iter_mut().take(bpp) {
            *slot = zero_hash();
        }
        let mut len = bpp;
        while len > 1 {
            for i in 0..(len / 2) {
                layer[i] = hash_pair(&layer[i * 2], &layer[i * 2 + 1]);
            }
            len /= 2;
        }
        return layer[0];
    }
    let mut layer = vec![zero_hash(); bpp];
    while layer.len() > 1 {
        let mut next = Vec::with_capacity(layer.len() / 2);
        for pair in layer.chunks_exact(2) {
            next.push(hash_pair(&pair[0], &pair[1]));
        }
        layer = next;
    }
    layer[0]
}

/// Rebuild the merkle root from piece-layer hashes for a file.
///
/// BEP 52: the piece layer contains `ceil(file_length / piece_length)` hashes.
/// The full tree has `next_power_of_two(num_pieces)` piece-level nodes.
/// Trailing nodes beyond the actual piece count are padded with the subtree
/// root computed from `blocks_per_piece` zero-hash leaves (NOT raw zero_hash).
///
/// Returns the computed merkle root (`pieces_root`).
pub fn root_from_piece_layer(
    piece_hashes: &[Id32],
    file_length: u64,
    piece_length: u32,
) -> crate::Result<Id32> {
    if piece_length < MERKLE_BLOCK_SIZE || !piece_length.is_power_of_two() {
        return Err(crate::Error::V2InvalidPieceLength(piece_length));
    }
    let num_pieces = file_length.div_ceil(piece_length as u64) as usize;
    if piece_hashes.len() != num_pieces {
        return Err(crate::Error::V2PieceLayerCountMismatch {
            expected: num_pieces,
            actual: piece_hashes.len(),
        });
    }

    if piece_hashes.len() == 1 {
        // Single-piece file: the piece hash IS the root.
        return Ok(piece_hashes[0]);
    }

    let blocks_per_piece = piece_length / MERKLE_BLOCK_SIZE;
    let pad_hash = padding_piece_hash(blocks_per_piece);

    // Pad to next power of 2.
    let padded_len = num_pieces.next_power_of_two();
    let mut layer: Vec<Id32> = Vec::with_capacity(padded_len);
    layer.extend_from_slice(piece_hashes);
    layer.resize(padded_len, pad_hash);

    // Build tree bottom-up.
    while layer.len() > 1 {
        let mut next = Vec::with_capacity(layer.len() / 2);
        for pair in layer.chunks_exact(2) {
            next.push(hash_pair(&pair[0], &pair[1]));
        }
        layer = next;
    }

    Ok(layer[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_zero_hash_is_all_zeros() {
        let z = zero_hash();
        assert_eq!(z.0, [0u8; 32]);
    }

    #[test]
    fn test_hash_block_deterministic() {
        let data = b"hello world";
        let h1 = hash_block(data);
        let h2 = hash_block(data);
        assert_eq!(h1, h2);
        // Should not be all zeros.
        assert_ne!(h1, zero_hash());
    }

    #[test]
    fn test_hash_pair_deterministic() {
        let a = hash_block(b"left");
        let b = hash_block(b"right");
        let h1 = hash_pair(&a, &b);
        let h2 = hash_pair(&a, &b);
        assert_eq!(h1, h2);
        // Order matters.
        let h3 = hash_pair(&b, &a);
        assert_ne!(h1, h3);
    }

    #[test]
    fn test_verify_piece_single_block() {
        // 1 block in a piece with blocks_per_piece=1 (piece_length == MERKLE_BLOCK_SIZE).
        let data = vec![0xABu8; MERKLE_BLOCK_SIZE as usize];
        let block_hash = hash_block(&data);
        // With bpp=1, the root is just the single leaf hash.
        assert!(verify_piece(&[block_hash], &block_hash, 1));
        assert!(!verify_piece(&[zero_hash()], &block_hash, 1));
    }

    #[test]
    fn test_verify_piece_two_blocks() {
        let data1 = vec![0x01u8; MERKLE_BLOCK_SIZE as usize];
        let data2 = vec![0x02u8; MERKLE_BLOCK_SIZE as usize];
        let h1 = hash_block(&data1);
        let h2 = hash_block(&data2);
        let expected_root = hash_pair(&h1, &h2);

        assert!(verify_piece(&[h1, h2], &expected_root, 2));
        // Wrong order should fail.
        assert!(!verify_piece(&[h2, h1], &expected_root, 2));
    }

    #[test]
    fn test_verify_piece_partial_blocks_padded() {
        // Piece has 3 actual blocks but blocks_per_piece=4 (padded to power of 2).
        let h1 = hash_block(b"block1");
        let h2 = hash_block(b"block2");
        let h3 = hash_block(b"block3");
        let z = zero_hash();

        // Tree:
        //        root
        //       /    \
        //    n01      n23
        //   / \      / \
        //  h1  h2   h3  z
        let n01 = hash_pair(&h1, &h2);
        let n23 = hash_pair(&h3, &z);
        let root = hash_pair(&n01, &n23);

        assert!(verify_piece(&[h1, h2, h3], &root, 4));
        // With all 4 explicit should also work.
        assert!(verify_piece(&[h1, h2, h3, z], &root, 4));
    }

    fn proof_for_leaf(mut idx: usize, mut layer: Vec<Id32>) -> Vec<Id32> {
        let mut proof = Vec::new();
        while layer.len() > 1 {
            let sibling = if idx & 1 == 0 {
                layer[idx + 1]
            } else {
                layer[idx - 1]
            };
            proof.push(sibling);

            let mut next = Vec::with_capacity(layer.len() / 2);
            for pair in layer.chunks_exact(2) {
                next.push(hash_pair(&pair[0], &pair[1]));
            }
            layer = next;
            idx >>= 1;
        }
        proof
    }

    #[test]
    fn test_verify_block_with_proof_valid() {
        let h0 = hash_block(b"b0");
        let h1 = hash_block(b"b1");
        let h2 = hash_block(b"b2");
        let h3 = hash_block(b"b3");

        let leaves = vec![h0, h1, h2, h3];
        let piece_root = hash_pair(&hash_pair(&h0, &h1), &hash_pair(&h2, &h3));
        let proof = proof_for_leaf(2, leaves);

        assert!(verify_block_with_proof(&h2, 2, &proof, &piece_root));
    }

    #[test]
    fn test_verify_block_with_proof_rejects_wrong_index() {
        let h0 = hash_block(b"b0");
        let h1 = hash_block(b"b1");
        let h2 = hash_block(b"b2");
        let h3 = hash_block(b"b3");

        let leaves = vec![h0, h1, h2, h3];
        let piece_root = hash_pair(&hash_pair(&h0, &h1), &hash_pair(&h2, &h3));
        let proof = proof_for_leaf(2, leaves);

        // Using the proof for index 2 but claiming index 1 should fail.
        assert!(!verify_block_with_proof(&h2, 1, &proof, &piece_root));
    }

    #[test]
    fn test_verify_piece_rejects_too_many_blocks() {
        let h1 = hash_block(b"block1");
        let h2 = hash_block(b"block2");
        let h3 = hash_block(b"block3");
        let h4 = hash_block(b"block4");
        let h5 = hash_block(b"block5");

        // blocks_per_piece=4 but 5 hashes provided -> should fail.
        let expected_root = hash_pair(&hash_pair(&h1, &h2), &hash_pair(&h3, &h4));
        assert!(!verify_piece(&[h1, h2, h3, h4, h5], &expected_root, 4));
    }

    #[test]
    fn test_compute_merkle_root_single_block() {
        // File with exactly 1 block (16 KiB), bpp=1 (piece_length == block_size).
        let h = hash_block(b"single block");
        let result = compute_merkle_root(&[h], 1);
        assert_eq!(result.root, h);
        assert_eq!(result.piece_hashes, vec![h]);
    }

    #[test]
    fn test_compute_merkle_root_bpp1_multiple_blocks() {
        // bpp=1 means each block is its own piece; piece layer == leaf layer.
        let h0 = hash_block(b"b0");
        let h1 = hash_block(b"b1");
        let h2 = hash_block(b"b2");

        let n01 = hash_pair(&h0, &h1);
        let n23 = hash_pair(&h2, &zero_hash());
        let expected_root = hash_pair(&n01, &n23);

        let result = compute_merkle_root(&[h0, h1, h2], 1);
        assert_eq!(result.root, expected_root);
        assert_eq!(result.piece_hashes, vec![h0, h1, h2]);
    }

    #[test]
    fn test_compute_merkle_root_bpp1_matches_root_from_piece_layer() {
        let h0 = hash_block(b"b0");
        let h1 = hash_block(b"b1");
        let h2 = hash_block(b"b2");
        let h3 = hash_block(b"b3");

        let blocks = [h0, h1, h2, h3];
        let result = compute_merkle_root(&blocks, 1);
        assert_eq!(result.piece_hashes.len(), blocks.len());

        let file_length = MERKLE_BLOCK_SIZE as u64 * blocks.len() as u64;
        let root2 =
            root_from_piece_layer(&result.piece_hashes, file_length, MERKLE_BLOCK_SIZE).unwrap();
        assert_eq!(result.root, root2);
    }

    #[test]
    fn test_compute_merkle_root_two_blocks_one_piece() {
        // File with 2 blocks, bpp=2 (piece_length = 32 KiB). Single piece.
        let h1 = hash_block(b"block0");
        let h2 = hash_block(b"block1");
        let expected_root = hash_pair(&h1, &h2);
        let result = compute_merkle_root(&[h1, h2], 2);
        assert_eq!(result.root, expected_root);
        assert_eq!(result.piece_hashes, vec![expected_root]);
    }

    #[test]
    fn test_compute_merkle_root_four_blocks_two_pieces() {
        // File with 4 blocks, bpp=2. Two pieces.
        let h0 = hash_block(b"b0");
        let h1 = hash_block(b"b1");
        let h2 = hash_block(b"b2");
        let h3 = hash_block(b"b3");

        let piece0 = hash_pair(&h0, &h1);
        let piece1 = hash_pair(&h2, &h3);
        let expected_root = hash_pair(&piece0, &piece1);

        let result = compute_merkle_root(&[h0, h1, h2, h3], 2);
        assert_eq!(result.root, expected_root);
        assert_eq!(result.piece_hashes, vec![piece0, piece1]);
    }

    #[test]
    fn test_compute_merkle_root_three_blocks_two_pieces_padded() {
        // File with 3 blocks, bpp=2. Two pieces, last piece has 1 data block + 1 zero pad.
        let h0 = hash_block(b"b0");
        let h1 = hash_block(b"b1");
        let h2 = hash_block(b"b2");
        let z = zero_hash();

        let piece0 = hash_pair(&h0, &h1);
        let piece1 = hash_pair(&h2, &z);
        let expected_root = hash_pair(&piece0, &piece1);

        let result = compute_merkle_root(&[h0, h1, h2], 2);
        assert_eq!(result.root, expected_root);
        assert_eq!(result.piece_hashes, vec![piece0, piece1]);
    }

    #[test]
    fn test_compute_merkle_root_five_blocks_three_pieces() {
        // File with 5 blocks, bpp=2. Three data pieces, padded to 4 pieces total.
        let h0 = hash_block(b"b0");
        let h1 = hash_block(b"b1");
        let h2 = hash_block(b"b2");
        let h3 = hash_block(b"b3");
        let h4 = hash_block(b"b4");
        let z = zero_hash();

        let piece0 = hash_pair(&h0, &h1);
        let piece1 = hash_pair(&h2, &h3);
        let piece2 = hash_pair(&h4, &z); // last data piece, padded
        let piece3 = hash_pair(&z, &z); // beyond-EOF padding piece

        let n01 = hash_pair(&piece0, &piece1);
        let n23 = hash_pair(&piece2, &piece3);
        let expected_root = hash_pair(&n01, &n23);

        let result = compute_merkle_root(&[h0, h1, h2, h3, h4], 2);
        assert_eq!(result.root, expected_root);
        // Only 3 data pieces, the 4th (beyond-EOF) is omitted.
        assert_eq!(result.piece_hashes, vec![piece0, piece1, piece2]);
    }

    #[test]
    fn test_compute_merkle_root_consistency_with_verify_piece() {
        // Ensure compute_merkle_root produces piece hashes that verify_piece accepts.
        let blocks: Vec<Id32> = (0..8)
            .map(|i| hash_block(&[i as u8; MERKLE_BLOCK_SIZE as usize]))
            .collect();
        let bpp = 4u32;
        let result = compute_merkle_root(&blocks, bpp);

        // Two pieces: blocks 0-3, blocks 4-7.
        assert_eq!(result.piece_hashes.len(), 2);

        assert!(verify_piece(&blocks[0..4], &result.piece_hashes[0], bpp));
        assert!(verify_piece(&blocks[4..8], &result.piece_hashes[1], bpp));
    }

    #[test]
    fn test_compute_merkle_root_consistency_with_root_from_piece_layer() {
        // Ensure compute_merkle_root's root matches root_from_piece_layer.
        let blocks: Vec<Id32> = (0..6).map(|i| hash_block(&[i as u8; 100])).collect();
        let bpp = 2u32;
        let result = compute_merkle_root(&blocks, bpp);

        // 6 blocks / 2 bpp = 3 data pieces.
        assert_eq!(result.piece_hashes.len(), 3);

        // file_length just needs to produce ceil(file_length / piece_length) == 3.
        let piece_length = bpp * MERKLE_BLOCK_SIZE;
        let file_length = (piece_length as u64) * 2 + 1; // 3 pieces
        let root2 = root_from_piece_layer(&result.piece_hashes, file_length, piece_length).unwrap();
        assert_eq!(result.root, root2);
    }

    #[test]
    fn test_root_from_piece_layer_single() {
        let h = hash_block(b"only piece");
        let root = root_from_piece_layer(&[h], 1000, 65536).unwrap();
        assert_eq!(root, h);
    }

    #[test]
    fn test_root_from_piece_layer_two_pieces() {
        let h1 = hash_block(b"piece0");
        let h2 = hash_block(b"piece1");
        let expected = hash_pair(&h1, &h2);
        // file_length = 2 * piece_length, so num_pieces = 2.
        let root = root_from_piece_layer(&[h1, h2], 131072, 65536).unwrap();
        assert_eq!(root, expected);
    }

    #[test]
    fn test_root_from_piece_layer_three_pieces() {
        let h1 = hash_block(b"p0");
        let h2 = hash_block(b"p1");
        let h3 = hash_block(b"p2");

        // piece_length=65536, blocks_per_piece=65536/16384=4
        // Padding piece hash is the subtree root of 4 zero-hash leaves.
        let pad = padding_piece_hash(4);

        // 3 pieces -> padded to 4.
        let n01 = hash_pair(&h1, &h2);
        let n23 = hash_pair(&h3, &pad);
        let expected = hash_pair(&n01, &n23);

        // file_length causes ceil(file_length / 65536) = 3.
        let root = root_from_piece_layer(&[h1, h2, h3], 65536 * 2 + 100, 65536).unwrap();
        assert_eq!(root, expected);
    }

    #[test]
    fn test_root_from_piece_layer_invalid_piece_length_rejected() {
        let h = hash_block(b"piece");
        let err = root_from_piece_layer(&[h], 1000, 8192).unwrap_err();
        assert!(matches!(err, crate::Error::V2InvalidPieceLength(8192)));
    }
}
