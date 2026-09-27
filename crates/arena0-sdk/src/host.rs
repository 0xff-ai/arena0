//! Pure Host imports: the result depends only on the arguments, so they are
//! allowed in every handler mode, agreed handlers compute them identically on
//! every participant, and a rerun after a crash repeats them. Native builds
//! (SDK and primitive unit tests) call `arena0_crypto` directly.

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "arena0")]
unsafe extern "C" {
    #[link_name = "hash"]
    fn import_hash(data_ptr: u32, data_len: u32, out_ptr: u32);
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

#[cfg(test)]
mod tests {
    use super::hash;

    #[test]
    fn hash_matches_the_blake3_empty_vector() {
        assert_eq!(
            hash(b""),
            [
                0xaf, 0x13, 0x49, 0xb9, 0xf5, 0xf9, 0xa1, 0xa6, 0xa0, 0x40, 0x4d, 0xea, 0x36, 0xdc,
                0xc9, 0x49, 0x9b, 0xcb, 0x25, 0xc9, 0xad, 0xc1, 0x12, 0xb7, 0xcc, 0x9a, 0x93, 0xca,
                0xe4, 0x1f, 0x32, 0x62,
            ]
        );
    }
}
