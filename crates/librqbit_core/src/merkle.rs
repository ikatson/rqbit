//! BEP 52 Merkle tree primitives for BitTorrent v2.
//!
//! Provides computation and verification of SHA-256 merkle trees used in
//! BEP 52 piece hashing. Files are divided into 16 KiB blocks, hashed with
//! SHA-256, and organized into a balanced binary tree.
//!
//! Key BEP 52 rules:
//! - Zero hash = all-zero bytes (NOT SHA-256 of empty data).
//! - Padding at the leaf level uses zero_hash(). Internal nodes are computed
//!   normally via hash_pair(). A "padding piece" at the piece layer is the
//!   subtree root of `blocks_per_piece` zero-hash leaves.
//! - Piece-layer hashes are trimmed: only actual (non-padding) pieces are kept.

use crate::Id32;
use sha1w::{ISha256, Sha256};

/// Size of each merkle tree leaf block (16 KiB), per BEP 52.
pub const MERKLE_BLOCK_SIZE: usize = 16384;
const SMALL_TREE_MAX_LEAVES: usize = 8;
const MAX_TREE_LEVELS: usize = u32::BITS as usize;

/// Errors from merkle tree operations.
#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub enum MerkleError {
    #[error("blocks_per_piece must not be zero")]
    ZeroBlocksPerPiece,
    #[error("blocks_per_piece must be a power of two, got {0}")]
    NonPowerOfTwoBlocksPerPiece(u32),
    #[error("block_hashes must not be empty")]
    EmptyBlockHashes,
    #[error("invalid proof length: expected {expected}, got {actual}")]
    InvalidProofLength { expected: usize, actual: usize },
    #[error("piece has too many blocks: max {max_blocks}, got {actual_blocks}")]
    PieceTooLarge {
        max_blocks: usize,
        actual_blocks: usize,
    },
    #[error("block index out of range: max {max_index}, got {actual_index}")]
    InvalidBlockIndex { max_index: u32, actual_index: u32 },
}

/// Result of computing a full merkle tree from block hashes.
#[derive(Debug)]
pub struct MerkleResult {
    /// The root hash of the complete merkle tree.
    pub root: Id32,
    /// Piece-layer hashes, one per actual piece. Trailing padding pieces are trimmed.
    pub piece_hashes: Vec<Id32>,
}

/// Returns the BEP 52 zero hash: all-zero bytes (NOT SHA-256 of empty data).
pub fn zero_hash() -> Id32 {
    Id32::default()
}

/// SHA-256 hash of a single block of file data (up to 16 KiB).
pub fn hash_block(data: &[u8]) -> Id32 {
    let mut hasher = Sha256::new();
    hasher.update(data);
    Id32::new(hasher.finish())
}

/// SHA-256(left || right) — combines two child nodes in the merkle tree.
pub fn hash_pair(left: &Id32, right: &Id32) -> Id32 {
    let mut hasher = Sha256::new();
    hasher.update(&left.0);
    hasher.update(&right.0);
    Id32::new(hasher.finish())
}

#[inline]
fn ct_eq_id32(left: &Id32, right: &Id32) -> bool {
    // Keep comparison branchless over all bytes.
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= left.0[i] ^ right.0[i];
    }
    diff == 0
}

fn validate_blocks_per_piece(blocks_per_piece: u32) -> Result<(), MerkleError> {
    if blocks_per_piece == 0 {
        return Err(MerkleError::ZeroBlocksPerPiece);
    }
    if !blocks_per_piece.is_power_of_two() {
        return Err(MerkleError::NonPowerOfTwoBlocksPerPiece(blocks_per_piece));
    }
    Ok(())
}

fn reduce_tree_in_place(current: &mut Vec<Id32>) {
    debug_assert!(!current.is_empty());
    debug_assert!(current.len().is_power_of_two());

    let mut len = current.len();
    while len > 1 {
        for i in 0..(len / 2) {
            // Read the current level from the upper range and write parent
            // hashes into the lower range of the same buffer.
            let left = current[i * 2];
            let right = current[i * 2 + 1];
            current[i] = hash_pair(&left, &right);
        }
        len /= 2;
        current.truncate(len);
    }
}

/// Build a balanced binary tree from power-of-2 leaves and return the root.
fn build_subtree_root_vec(leaves: &[Id32]) -> Id32 {
    debug_assert!(!leaves.is_empty());
    debug_assert!(leaves.len().is_power_of_two());

    if leaves.len() == 1 {
        return leaves[0];
    }

    let mut current = leaves.to_vec();
    reduce_tree_in_place(&mut current);
    current[0]
}

/// Stack-based root builder for small trees (<= 8 leaves).
fn build_subtree_root_small(leaves: &[Id32]) -> Id32 {
    debug_assert!(!leaves.is_empty());
    debug_assert!(leaves.len().is_power_of_two());
    debug_assert!(leaves.len() <= SMALL_TREE_MAX_LEAVES);

    let mut current = [Id32::default(); SMALL_TREE_MAX_LEAVES];
    current[..leaves.len()].copy_from_slice(leaves);
    let mut len = leaves.len();

    while len > 1 {
        for i in 0..(len / 2) {
            current[i] = hash_pair(&current[i * 2], &current[i * 2 + 1]);
        }
        len /= 2;
    }

    current[0]
}

fn build_subtree_root(leaves: &[Id32]) -> Id32 {
    if leaves.len() <= SMALL_TREE_MAX_LEAVES {
        return build_subtree_root_small(leaves);
    }
    build_subtree_root_vec(leaves)
}

fn piece_layer_from_block_hashes_streaming(
    block_hashes: impl IntoIterator<Item = Id32>,
    blocks_per_piece: usize,
) -> Result<Vec<Id32>, MerkleError> {
    let mut piece_hashes = Vec::new();
    let mut piece_leaves = vec![zero_hash(); blocks_per_piece];
    let mut it = block_hashes.into_iter();

    loop {
        let mut blocks_in_piece = 0usize;
        while blocks_in_piece < blocks_per_piece {
            match it.next() {
                Some(h) => {
                    piece_leaves[blocks_in_piece] = h;
                    blocks_in_piece += 1;
                }
                None => break,
            }
        }

        if blocks_in_piece == 0 {
            break;
        }

        if blocks_in_piece < blocks_per_piece {
            piece_leaves[blocks_in_piece..].fill(zero_hash());
        }

        piece_hashes.push(build_subtree_root(&piece_leaves));

        if blocks_in_piece < blocks_per_piece {
            // Input is exhausted. This was the final (possibly short) piece.
            break;
        }
    }

    if piece_hashes.is_empty() {
        return Err(MerkleError::EmptyBlockHashes);
    }

    Ok(piece_hashes)
}

/// Streaming merkle construction from block hashes.
///
/// Processes input lazily in piece-sized groups and avoids materializing the
/// full block-hash list in memory.
pub fn compute_merkle_root_streaming(
    block_hashes: impl IntoIterator<Item = Id32>,
    blocks_per_piece: u32,
) -> Result<MerkleResult, MerkleError> {
    validate_blocks_per_piece(blocks_per_piece)?;
    let piece_hashes =
        piece_layer_from_block_hashes_streaming(block_hashes, blocks_per_piece as usize)?;
    let root = root_from_piece_layer(&piece_hashes, blocks_per_piece)?;
    Ok(MerkleResult { root, piece_hashes })
}

/// Compute the full merkle tree from precomputed block hashes.
///
/// This is a compatibility wrapper around [`compute_merkle_root_streaming()`]
/// for callers that already have a slice in memory.
pub fn compute_merkle_root(
    block_hashes: &[Id32],
    blocks_per_piece: u32,
) -> Result<MerkleResult, MerkleError> {
    compute_merkle_root_streaming(block_hashes.iter().copied(), blocks_per_piece)
}

fn root_from_power_of_two_leaves_noalloc(
    leaves: impl IntoIterator<Item = Id32>,
    leaf_count: usize,
) -> Id32 {
    debug_assert!(leaf_count > 0);
    debug_assert!(leaf_count.is_power_of_two());

    let mut partial = [Id32::default(); MAX_TREE_LEVELS];
    let mut occupied = [false; MAX_TREE_LEVELS];
    let mut processed = 0usize;

    for mut node in leaves {
        let mut level = 0usize;
        let mut carry = processed;

        while carry & 1 == 1 {
            debug_assert!(occupied[level]);
            node = hash_pair(&partial[level], &node);
            occupied[level] = false;
            carry >>= 1;
            level += 1;
        }

        partial[level] = node;
        occupied[level] = true;
        processed += 1;
    }

    debug_assert_eq!(processed, leaf_count);
    let root_level = leaf_count.trailing_zeros() as usize;
    debug_assert!(occupied[root_level]);
    partial[root_level]
}

/// Verify a piece by hashing its data into blocks, building the subtree, and
/// comparing the root against the expected piece hash.
///
/// `piece_data` may be shorter than a full piece (last piece of a file).
/// Missing blocks are padded with [`zero_hash()`].
pub fn verify_piece(
    piece_data: &[u8],
    blocks_per_piece: u32,
    expected_hash: &Id32,
) -> Result<bool, MerkleError> {
    validate_blocks_per_piece(blocks_per_piece)?;

    let bpp = blocks_per_piece as usize;
    let actual_blocks = piece_data.len().div_ceil(MERKLE_BLOCK_SIZE);
    if actual_blocks > bpp {
        return Err(MerkleError::PieceTooLarge {
            max_blocks: bpp,
            actual_blocks,
        });
    }

    // Keep validation allocation-free: stream real leaves first, then virtual
    // zero-hash leaves up to blocks_per_piece.
    let leaves = piece_data
        .chunks(MERKLE_BLOCK_SIZE)
        .map(hash_block)
        .chain(std::iter::repeat_with(zero_hash).take(bpp.saturating_sub(actual_blocks)));

    let root = root_from_power_of_two_leaves_noalloc(leaves, bpp);
    Ok(ct_eq_id32(&root, expected_hash))
}

/// Verify a single block using a merkle proof (bottom-up sibling hashes).
///
/// Proof ordering is bottom-up: `proof[0]` is the sibling at the leaf level,
/// `proof[1]` at the next level up, etc. `block_index_in_piece` determines
/// left/right placement at each level (bit 0 = leaf level, bit 1 = next, ...).
pub fn verify_block_with_proof(
    block_data: &[u8],
    block_index_in_piece: u32,
    proof: &[Id32],
    expected_piece_hash: &Id32,
    blocks_per_piece: u32,
) -> Result<bool, MerkleError> {
    validate_blocks_per_piece(blocks_per_piece)?;
    if block_index_in_piece >= blocks_per_piece {
        return Err(MerkleError::InvalidBlockIndex {
            max_index: blocks_per_piece - 1,
            actual_index: block_index_in_piece,
        });
    }

    let expected_proof_len = blocks_per_piece.trailing_zeros() as usize;
    if proof.len() != expected_proof_len {
        return Err(MerkleError::InvalidProofLength {
            expected: expected_proof_len,
            actual: proof.len(),
        });
    }

    let mut current = hash_block(block_data);
    let mut index = block_index_in_piece;

    for sibling in proof {
        if index & 1 == 0 {
            current = hash_pair(&current, sibling);
        } else {
            current = hash_pair(sibling, &current);
        }
        index >>= 1;
    }

    Ok(ct_eq_id32(&current, expected_piece_hash))
}

/// Compute the piece-layer hash for a "padding piece" — a piece composed
/// entirely of [`zero_hash()`] leaves.
///
/// For `blocks_per_piece == 1`, returns `zero_hash()`.
/// For larger values, returns the subtree root of `blocks_per_piece` zero-hash
/// leaves. This is NOT the same as `zero_hash()` when `blocks_per_piece > 1`.
pub fn padding_piece_hash(blocks_per_piece: u32) -> Result<Id32, MerkleError> {
    validate_blocks_per_piece(blocks_per_piece)?;

    if blocks_per_piece == 1 {
        return Ok(zero_hash());
    }

    let bpp = blocks_per_piece as usize;
    if bpp <= SMALL_TREE_MAX_LEAVES {
        let zero = zero_hash();
        let leaves = [zero; SMALL_TREE_MAX_LEAVES];
        return Ok(build_subtree_root_small(&leaves[..bpp]));
    }

    let leaves = vec![zero_hash(); bpp];
    Ok(build_subtree_root(&leaves))
}

/// Rebuild the merkle root from piece-layer hashes.
///
/// Pads the piece layer to a power of 2 using [`padding_piece_hash()`], then
/// builds the tree from the piece layer up to the root.
///
/// This is the inverse direction of [`compute_merkle_root()`]: given the piece
/// layer extracted from a torrent file, reconstruct the file's merkle root
/// to validate against `pieces_root`.
pub fn root_from_piece_layer(
    piece_hashes: &[Id32],
    blocks_per_piece: u32,
) -> Result<Id32, MerkleError> {
    if piece_hashes.is_empty() {
        return Err(MerkleError::EmptyBlockHashes);
    }
    validate_blocks_per_piece(blocks_per_piece)?;

    let pad_hash = padding_piece_hash(blocks_per_piece)?;
    let n_padded = piece_hashes.len().next_power_of_two();

    if n_padded <= SMALL_TREE_MAX_LEAVES {
        let mut current = [Id32::default(); SMALL_TREE_MAX_LEAVES];
        current[..piece_hashes.len()].copy_from_slice(piece_hashes);
        current[piece_hashes.len()..n_padded].fill(pad_hash);
        let mut len = n_padded;

        while len > 1 {
            for i in 0..(len / 2) {
                current[i] = hash_pair(&current[i * 2], &current[i * 2 + 1]);
            }
            len /= 2;
        }

        return Ok(current[0]);
    }

    let mut current: Vec<Id32> = Vec::with_capacity(n_padded);
    current.extend_from_slice(piece_hashes);
    current.resize(n_padded, pad_hash);

    reduce_tree_in_place(&mut current);

    Ok(current[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_block(byte: u8) -> Vec<u8> {
        vec![byte; MERKLE_BLOCK_SIZE]
    }

    // --- Core primitives ---

    #[test]
    fn test_zero_hash_is_all_zeros() {
        let zh = zero_hash();
        assert_eq!(zh.0, [0u8; 32]);
    }

    #[test]
    fn test_ct_eq_id32() {
        let a = hash_block(&make_block(1));
        let b = a;
        let c = hash_block(&make_block(2));
        assert!(ct_eq_id32(&a, &b));
        assert!(!ct_eq_id32(&a, &c));
    }

    #[test]
    fn test_hash_block_deterministic() {
        let data = make_block(0xAB);
        let h1 = hash_block(&data);
        let h2 = hash_block(&data);
        assert_eq!(h1, h2);
        assert_ne!(h1, zero_hash());
    }

    #[test]
    fn test_hash_pair_deterministic() {
        let a = hash_block(&make_block(1));
        let b = hash_block(&make_block(2));
        let h1 = hash_pair(&a, &b);
        let h2 = hash_pair(&a, &b);
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_hash_pair_not_commutative() {
        let a = hash_block(&make_block(1));
        let b = hash_block(&make_block(2));
        assert_ne!(hash_pair(&a, &b), hash_pair(&b, &a));
    }

    #[test]
    fn test_build_subtree_root_small_matches_vec_path() {
        let leaves: Vec<Id32> = (0..8).map(|i| hash_block(&make_block(i))).collect();
        assert_eq!(
            build_subtree_root_small(&leaves),
            build_subtree_root_vec(&leaves)
        );
    }

    // --- compute_merkle_root ---

    #[test]
    fn test_single_block_bpp1() -> Result<(), MerkleError> {
        let h = hash_block(&make_block(0));
        let result = compute_merkle_root(&[h], 1)?;
        assert_eq!(result.root, h);
        assert_eq!(result.piece_hashes, vec![h]);
        Ok(())
    }

    #[test]
    fn test_two_blocks_bpp2() -> Result<(), MerkleError> {
        let h0 = hash_block(&make_block(0));
        let h1 = hash_block(&make_block(1));
        let expected_root = hash_pair(&h0, &h1);

        let result = compute_merkle_root(&[h0, h1], 2)?;
        assert_eq!(result.root, expected_root);
        assert_eq!(result.piece_hashes, vec![expected_root]);
        Ok(())
    }

    #[test]
    fn test_four_blocks_two_pieces_bpp2() -> Result<(), MerkleError> {
        let h0 = hash_block(&make_block(0));
        let h1 = hash_block(&make_block(1));
        let h2 = hash_block(&make_block(2));
        let h3 = hash_block(&make_block(3));

        let piece0 = hash_pair(&h0, &h1);
        let piece1 = hash_pair(&h2, &h3);
        let expected_root = hash_pair(&piece0, &piece1);

        let result = compute_merkle_root(&[h0, h1, h2, h3], 2)?;
        assert_eq!(result.root, expected_root);
        assert_eq!(result.piece_hashes, vec![piece0, piece1]);
        Ok(())
    }

    #[test]
    fn test_three_blocks_bpp2_padding() -> Result<(), MerkleError> {
        // 3 blocks, bpp=2: 2 pieces (piece 1 has 1 real block + 1 zero pad)
        let h0 = hash_block(&make_block(0));
        let h1 = hash_block(&make_block(1));
        let h2 = hash_block(&make_block(2));
        let zh = zero_hash();

        let piece0 = hash_pair(&h0, &h1);
        let piece1 = hash_pair(&h2, &zh);
        let expected_root = hash_pair(&piece0, &piece1);

        let result = compute_merkle_root(&[h0, h1, h2], 2)?;
        assert_eq!(result.root, expected_root);
        assert_eq!(result.piece_hashes.len(), 2);
        assert_eq!(result.piece_hashes, vec![piece0, piece1]);
        Ok(())
    }

    #[test]
    fn test_piece_layer_trimming_9_blocks_bpp4() -> Result<(), MerkleError> {
        // 9 blocks, bpp=4 → 3 piece hashes (not 4)
        let hashes: Vec<Id32> = (0..9).map(|i| hash_block(&make_block(i))).collect();

        let result = compute_merkle_root(&hashes, 4)?;
        assert_eq!(
            result.piece_hashes.len(),
            3,
            "9 blocks with bpp=4 should produce 3 piece hashes, not 4"
        );
        Ok(())
    }

    #[test]
    fn test_compute_merkle_root_streaming_matches_slice_api() -> Result<(), MerkleError> {
        let hashes: Vec<Id32> = (0..9).map(|i| hash_block(&make_block(i))).collect();
        let from_slice = compute_merkle_root(&hashes, 4)?;
        let from_stream = compute_merkle_root_streaming(hashes.into_iter(), 4)?;
        assert_eq!(from_slice.root, from_stream.root);
        assert_eq!(from_slice.piece_hashes, from_stream.piece_hashes);
        Ok(())
    }

    #[test]
    fn test_compute_merkle_root_streaming_without_hash_vec() -> Result<(), MerkleError> {
        let result = compute_merkle_root_streaming((0..9).map(|i| hash_block(&make_block(i))), 4)?;
        assert_eq!(result.piece_hashes.len(), 3);
        Ok(())
    }

    #[test]
    fn test_single_block_bpp4() -> Result<(), MerkleError> {
        // 1 block with bpp=4: single piece, 3 zero-hash padding leaves
        let h = hash_block(&make_block(42));
        let zh = zero_hash();

        let n01 = hash_pair(&h, &zh);
        let n23 = hash_pair(&zh, &zh);
        let expected_root = hash_pair(&n01, &n23);

        let result = compute_merkle_root(&[h], 4)?;
        assert_eq!(result.root, expected_root);
        assert_eq!(result.piece_hashes.len(), 1);
        assert_eq!(result.piece_hashes[0], expected_root);
        Ok(())
    }

    #[test]
    fn test_bpp1_multiple_blocks() -> Result<(), MerkleError> {
        // bpp=1: each block is its own piece
        let h0 = hash_block(&make_block(0));
        let h1 = hash_block(&make_block(1));

        let result = compute_merkle_root(&[h0, h1], 1)?;
        assert_eq!(result.piece_hashes, vec![h0, h1]);
        assert_eq!(result.root, hash_pair(&h0, &h1));
        Ok(())
    }

    // --- verify_piece ---

    #[test]
    fn test_verify_piece_valid() -> Result<(), MerkleError> {
        let block0 = make_block(10);
        let block1 = make_block(20);
        let mut piece_data = block0.clone();
        piece_data.extend_from_slice(&block1);

        let h0 = hash_block(&block0);
        let h1 = hash_block(&block1);
        let expected = hash_pair(&h0, &h1);

        assert!(verify_piece(&piece_data, 2, &expected)?);
        Ok(())
    }

    #[test]
    fn test_verify_piece_invalid() -> Result<(), MerkleError> {
        let piece_data = make_block(10);
        let wrong_hash = hash_block(&make_block(99));

        assert!(!verify_piece(&piece_data, 1, &wrong_hash)?);
        Ok(())
    }

    #[test]
    fn test_verify_piece_last_piece_shorter() -> Result<(), MerkleError> {
        // Last piece with fewer blocks than bpp: 1 real block out of bpp=4
        let block = make_block(7);
        let h = hash_block(&block);
        let zh = zero_hash();

        let n01 = hash_pair(&h, &zh);
        let n23 = hash_pair(&zh, &zh);
        let expected = hash_pair(&n01, &n23);

        assert!(verify_piece(&block, 4, &expected)?);
        Ok(())
    }

    // --- verify_block_with_proof ---

    #[test]
    fn test_verify_block_with_proof_valid() -> Result<(), MerkleError> {
        // Build a 4-leaf tree manually and verify block 0 with proof
        let blocks: Vec<Vec<u8>> = (0..4).map(make_block).collect();
        let h: Vec<Id32> = blocks.iter().map(|b| hash_block(b)).collect();

        let n01 = hash_pair(&h[0], &h[1]);
        let n23 = hash_pair(&h[2], &h[3]);
        let root = hash_pair(&n01, &n23);

        // Proof for block 0: [h[1], n23]
        let proof = vec![h[1], n23];
        assert!(verify_block_with_proof(&blocks[0], 0, &proof, &root, 4)?);

        // Proof for block 2: [h[3], n01]
        let proof2 = vec![h[3], n01];
        assert!(verify_block_with_proof(&blocks[2], 2, &proof2, &root, 4)?);

        // Proof for block 1: [h[0], n23]
        let proof1 = vec![h[0], n23];
        assert!(verify_block_with_proof(&blocks[1], 1, &proof1, &root, 4)?);

        // Proof for block 3: [h[2], n01]
        let proof3 = vec![h[2], n01];
        assert!(verify_block_with_proof(&blocks[3], 3, &proof3, &root, 4)?);

        Ok(())
    }

    #[test]
    fn test_verify_block_with_proof_wrong_data() -> Result<(), MerkleError> {
        let blocks: Vec<Vec<u8>> = (0..4).map(make_block).collect();
        let h: Vec<Id32> = blocks.iter().map(|b| hash_block(b)).collect();

        let n01 = hash_pair(&h[0], &h[1]);
        let n23 = hash_pair(&h[2], &h[3]);
        let root = hash_pair(&n01, &n23);

        // Correct proof for block 0, but with wrong block data
        let proof = vec![h[1], n23];
        let wrong_data = make_block(99);
        assert!(!verify_block_with_proof(&wrong_data, 0, &proof, &root, 4)?);
        Ok(())
    }

    #[test]
    fn test_verify_block_with_proof_wrong_order() -> Result<(), MerkleError> {
        // Reversed proof order should fail
        let blocks: Vec<Vec<u8>> = (0..4).map(make_block).collect();
        let h: Vec<Id32> = blocks.iter().map(|b| hash_block(b)).collect();

        let n01 = hash_pair(&h[0], &h[1]);
        let n23 = hash_pair(&h[2], &h[3]);
        let root = hash_pair(&n01, &n23);

        // Correct proof for block 0 is [h[1], n23]; reversed is [n23, h[1]]
        let reversed_proof = vec![n23, h[1]];
        assert!(!verify_block_with_proof(
            &blocks[0],
            0,
            &reversed_proof,
            &root,
            4
        )?);
        Ok(())
    }

    #[test]
    fn test_verify_block_with_proof_out_of_range_index() {
        // Build a valid 4-block piece and proof for block 0.
        let blocks: Vec<Vec<u8>> = (0..4).map(make_block).collect();
        let h: Vec<Id32> = blocks.iter().map(|b| hash_block(b)).collect();

        let n01 = hash_pair(&h[0], &h[1]);
        let n23 = hash_pair(&h[2], &h[3]);
        let root = hash_pair(&n01, &n23);
        let proof_for_block0 = vec![h[1], n23];

        // Index 4 is out of range for bpp=4 (valid indices are 0..=3).
        // Without range validation this could alias a valid path after shifts.
        assert_eq!(
            verify_block_with_proof(&blocks[0], 4, &proof_for_block0, &root, 4).unwrap_err(),
            MerkleError::InvalidBlockIndex {
                max_index: 3,
                actual_index: 4
            }
        );
    }

    // --- padding_piece_hash ---

    #[test]
    fn test_padding_piece_hash_bpp1() -> Result<(), MerkleError> {
        assert_eq!(padding_piece_hash(1)?, zero_hash());
        Ok(())
    }

    #[test]
    fn test_padding_piece_hash_bpp2() -> Result<(), MerkleError> {
        let zh = zero_hash();
        let expected = hash_pair(&zh, &zh);
        assert_eq!(padding_piece_hash(2)?, expected);
        // Must NOT equal zero_hash for bpp > 1
        assert_ne!(padding_piece_hash(2)?, zero_hash());
        Ok(())
    }

    #[test]
    fn test_padding_piece_hash_bpp4() -> Result<(), MerkleError> {
        let zh = zero_hash();
        let n01 = hash_pair(&zh, &zh);
        let n23 = hash_pair(&zh, &zh);
        let expected = hash_pair(&n01, &n23);
        assert_eq!(padding_piece_hash(4)?, expected);
        assert_ne!(padding_piece_hash(4)?, zero_hash());
        assert_ne!(padding_piece_hash(4)?, padding_piece_hash(2)?);
        Ok(())
    }

    // --- root_from_piece_layer ---

    #[test]
    fn test_root_from_piece_layer_single() -> Result<(), MerkleError> {
        let h = hash_block(&make_block(0));
        assert_eq!(root_from_piece_layer(&[h], 1)?, h);
        Ok(())
    }

    #[test]
    fn test_root_from_piece_layer_two_pieces() -> Result<(), MerkleError> {
        let p0 = hash_block(&make_block(0));
        let p1 = hash_block(&make_block(1));
        let expected = hash_pair(&p0, &p1);
        assert_eq!(root_from_piece_layer(&[p0, p1], 2)?, expected);
        Ok(())
    }

    #[test]
    fn test_root_from_piece_layer_cross_validates_compute() -> Result<(), MerkleError> {
        // Build a tree with compute_merkle_root, then reconstruct from piece layer
        let hashes: Vec<Id32> = (0..8).map(|i| hash_block(&make_block(i))).collect();
        let result = compute_merkle_root(&hashes, 2)?;

        let reconstructed = root_from_piece_layer(&result.piece_hashes, 2)?;
        assert_eq!(
            reconstructed, result.root,
            "root_from_piece_layer must match compute_merkle_root"
        );
        Ok(())
    }

    #[test]
    fn test_root_from_piece_layer_cross_validates_with_padding() -> Result<(), MerkleError> {
        // 5 blocks, bpp=2 → 3 piece hashes, padded to 4 at piece layer
        let hashes: Vec<Id32> = (0..5).map(|i| hash_block(&make_block(i))).collect();
        let result = compute_merkle_root(&hashes, 2)?;
        assert_eq!(result.piece_hashes.len(), 3);

        let reconstructed = root_from_piece_layer(&result.piece_hashes, 2)?;
        assert_eq!(reconstructed, result.root);
        Ok(())
    }

    #[test]
    fn test_root_from_piece_layer_large_cross_validation() -> Result<(), MerkleError> {
        // 33 blocks, bpp=4 → 9 piece hashes
        let hashes: Vec<Id32> = (0..33).map(|i| hash_block(&make_block(i as u8))).collect();
        let result = compute_merkle_root(&hashes, 4)?;
        assert_eq!(result.piece_hashes.len(), 9);

        let reconstructed = root_from_piece_layer(&result.piece_hashes, 4)?;
        assert_eq!(reconstructed, result.root);
        Ok(())
    }

    // --- Error cases ---

    #[test]
    fn test_error_zero_blocks_per_piece() {
        assert_eq!(
            compute_merkle_root(&[zero_hash()], 0).unwrap_err(),
            MerkleError::ZeroBlocksPerPiece
        );
        assert_eq!(
            verify_piece(&[], 0, &zero_hash()).unwrap_err(),
            MerkleError::ZeroBlocksPerPiece
        );
        assert_eq!(
            padding_piece_hash(0).unwrap_err(),
            MerkleError::ZeroBlocksPerPiece
        );
        assert_eq!(
            root_from_piece_layer(&[zero_hash()], 0).unwrap_err(),
            MerkleError::ZeroBlocksPerPiece
        );
    }

    #[test]
    fn test_error_non_power_of_two_bpp() {
        assert_eq!(
            compute_merkle_root(&[zero_hash()], 3).unwrap_err(),
            MerkleError::NonPowerOfTwoBlocksPerPiece(3)
        );
        assert_eq!(
            verify_piece(&[], 5, &zero_hash()).unwrap_err(),
            MerkleError::NonPowerOfTwoBlocksPerPiece(5)
        );
        assert_eq!(
            padding_piece_hash(6).unwrap_err(),
            MerkleError::NonPowerOfTwoBlocksPerPiece(6)
        );
        assert_eq!(
            root_from_piece_layer(&[zero_hash()], 7).unwrap_err(),
            MerkleError::NonPowerOfTwoBlocksPerPiece(7)
        );
    }

    #[test]
    fn test_error_empty_block_hashes() {
        assert_eq!(
            compute_merkle_root(&[], 1).unwrap_err(),
            MerkleError::EmptyBlockHashes
        );
        assert_eq!(
            root_from_piece_layer(&[], 1).unwrap_err(),
            MerkleError::EmptyBlockHashes
        );
    }

    #[test]
    fn test_error_invalid_proof_length() {
        let h = hash_block(&make_block(0));
        // bpp=4 requires proof of length 2
        assert_eq!(
            verify_block_with_proof(&make_block(0), 0, &[h], &h, 4).unwrap_err(),
            MerkleError::InvalidProofLength {
                expected: 2,
                actual: 1
            }
        );
        assert_eq!(
            verify_block_with_proof(&make_block(0), 0, &[h, h, h], &h, 4).unwrap_err(),
            MerkleError::InvalidProofLength {
                expected: 2,
                actual: 3
            }
        );
    }

    #[test]
    fn test_error_piece_too_large() {
        // bpp=2 allows at most 2 blocks. Provide 3 blocks.
        let mut piece = make_block(1);
        piece.extend_from_slice(&make_block(2));
        piece.extend_from_slice(&make_block(3));

        assert_eq!(
            verify_piece(&piece, 2, &zero_hash()).unwrap_err(),
            MerkleError::PieceTooLarge {
                max_blocks: 2,
                actual_blocks: 3
            }
        );
    }

    #[test]
    fn test_verify_block_bpp1_no_proof() -> Result<(), MerkleError> {
        // bpp=1: proof length is 0, the block hash IS the piece hash
        let block = make_block(42);
        let h = hash_block(&block);
        assert!(verify_block_with_proof(&block, 0, &[], &h, 1)?);
        Ok(())
    }
}
