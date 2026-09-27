//! BLAKE3's tree hash, exposed one node at a time.
//!
//! BLAKE3 hashes 1 KiB chunks into a left-balanced binary tree: for `n > 1`
//! chunks the left subtree holds the largest power of two of chunks smaller
//! than `n`. A subtree's chaining value depends on its bytes and their offset;
//! merging two sibling chaining values yields their parent's, and merging the
//! root's children with `root = true` yields `blake3::hash` of the whole input.
//! Callers must respect the tree shape; these functions check only what
//! `blake3::hazmat` would otherwise panic on.

use blake3::hazmat::{self, HasherExt as _, Mode};

/// BLAKE3's chunk length. Subtree offsets are multiples of it.
pub const CHUNK_BYTES: u64 = blake3::CHUNK_LEN as u64;

/// The chaining value of `bytes` as the subtree starting at byte `offset`.
///
/// # Panics
///
/// If `bytes` is empty, `offset` is not a multiple of [`CHUNK_BYTES`], or
/// `bytes` is longer than the subtree that can start at `offset`
/// ([`is_subtree`] returns `false`).
#[must_use]
pub fn subtree_cv(bytes: &[u8], offset: u64) -> [u8; 32] {
    assert!(
        is_subtree(offset, bytes.len() as u64),
        "not a BLAKE3 subtree"
    );
    let mut hasher = blake3::Hasher::new();
    hasher.set_input_offset(offset);
    hasher.update(bytes);
    hasher.finalize_non_root()
}

/// Whether `len` bytes starting at `offset` can form one BLAKE3 subtree:
/// nonempty, chunk-aligned, and no longer than `hazmat::max_subtree_len`.
#[must_use]
pub fn is_subtree(offset: u64, len: u64) -> bool {
    len != 0
        && offset.is_multiple_of(CHUNK_BYTES)
        && hazmat::max_subtree_len(offset).is_none_or(|max| len <= max)
}

/// The parent of two sibling chaining values: a chaining value, or the root
/// hash when `root` is true.
#[must_use]
pub fn merge_cv(left: &[u8; 32], right: &[u8; 32], root: bool) -> [u8; 32] {
    if root {
        *hazmat::merge_subtrees_root(left, right, Mode::Hash).as_bytes()
    } else {
        hazmat::merge_subtrees_non_root(left, right, Mode::Hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEAF: usize = 32 * 1024;

    /// The root of `leaves` (each a `LEAF`-aligned subtree) by BLAKE3's
    /// left-balanced recursion.
    fn root(leaves: &[[u8; 32]]) -> [u8; 32] {
        fn node(leaves: &[[u8; 32]], root: bool) -> [u8; 32] {
            // The largest power of two smaller than the leaf count.
            let left = 1 << (leaves.len() - 1).ilog2();
            merge_cv(&subtree(&leaves[..left]), &subtree(&leaves[left..]), root)
        }
        fn subtree(leaves: &[[u8; 32]]) -> [u8; 32] {
            if leaves.len() == 1 {
                leaves[0]
            } else {
                node(leaves, false)
            }
        }
        node(leaves, true)
    }

    #[test]
    fn leaf_merges_reproduce_the_blake3_hash() {
        for (count, tail) in [
            (2, LEAF),
            (2, 1),
            (3, 7),
            (5, LEAF),
            (6, 2048),
            (7, 3),
            (8, 1000),
            (9, 1),
            (512, 12_345),
        ] {
            let len = (count - 1) * LEAF + tail;
            let input: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let leaves: Vec<[u8; 32]> = input
                .chunks(LEAF)
                .enumerate()
                .map(|(i, leaf)| subtree_cv(leaf, (i * LEAF) as u64))
                .collect();
            assert_eq!(leaves.len(), count);
            assert_eq!(
                root(&leaves),
                *blake3::hash(&input).as_bytes(),
                "{count} leaves"
            );
        }
    }

    #[test]
    fn subtree_bounds_follow_the_tree() {
        assert!(is_subtree(0, u64::MAX));
        assert!(is_subtree(LEAF as u64, LEAF as u64));
        assert!(!is_subtree(LEAF as u64, LEAF as u64 + 1));
        assert!(!is_subtree(1, 1));
        assert!(!is_subtree(0, 0));
    }
}
