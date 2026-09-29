//! Pure Host imports: the result depends only on the arguments, so they are
//! allowed in every handler mode, agreed handlers compute them identically on
//! every participant, and a rerun after a crash repeats them. Native builds
//! (SDK and primitive unit tests) call `arena0_crypto` directly.

use arena0_protocol::ChainingValue;

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "arena0")]
unsafe extern "C" {
    #[link_name = "hash"]
    fn import_hash(data_ptr: u32, data_len: u32, out_ptr: u32);
    #[link_name = "merge_cv"]
    fn import_merge_cv(left_ptr: u32, right_ptr: u32, root: u32, out_ptr: u32);
    #[link_name = "permutation"]
    fn import_permutation(seed_ptr: u32, n: u32, out_ptr: u32);
}

/// BLAKE3 of `data`, computed by the Host.
#[must_use]
pub fn hash(data: &[u8]) -> [u8; 32] {
    #[cfg(target_arch = "wasm32")]
    {
        let mut out = [0u8; 32];
        // SAFETY: both buffers remain live for the call; the Host writes exactly 32 bytes.
        unsafe {
            import_hash(
                data.as_ptr() as u32,
                data.len() as u32,
                out.as_mut_ptr() as u32,
            )
        };
        out
    }
    #[cfg(not(target_arch = "wasm32"))]
    arena0_crypto::hash(arena0_crypto::HashAlgorithm::Blake3, data)
}

/// The BLAKE3 parent of two sibling chaining values, computed by the Host: a
/// chaining value, or the root hash when `root` is true. See
/// [`crate::Blobs::subtree_cv`] for the leaves.
#[must_use]
pub fn merge_cv(left: &ChainingValue, right: &ChainingValue, root: bool) -> ChainingValue {
    #[cfg(target_arch = "wasm32")]
    {
        let mut out = [0u8; 32];
        // SAFETY: all three buffers remain live for the call; the Host reads
        // 32 bytes from each input and writes exactly 32 bytes.
        unsafe {
            import_merge_cv(
                left.as_ptr() as u32,
                right.as_ptr() as u32,
                u32::from(root),
                out.as_mut_ptr() as u32,
            )
        };
        out
    }
    #[cfg(not(target_arch = "wasm32"))]
    arena0_crypto::blake3_tree::merge_cv(left, right, root)
}

/// The Fisher-Yates permutation of `0..n` drawn from ChaCha20 seeded with
/// `seed`, computed by the Host. The Host traps when `n` exceeds the profile's
/// `max_permutation_len` (4,096).
#[must_use]
pub fn permutation(seed: [u8; 32], n: u32) -> Vec<u32> {
    #[cfg(target_arch = "wasm32")]
    {
        let mut out = vec![0u32; n as usize];
        // SAFETY: the seed and output remain live for the call. The Host writes
        // n little-endian u32s, matching Wasm's native byte order.
        unsafe { import_permutation(seed.as_ptr() as u32, n, out.as_mut_ptr() as u32) };
        out
    }
    #[cfg(not(target_arch = "wasm32"))]
    arena0_crypto::permutation(seed, n).expect("permutation draw budget exhausted")
}
