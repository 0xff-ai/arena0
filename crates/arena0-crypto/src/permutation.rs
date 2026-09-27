//! Seeded permutations for Host imports and native program tests.

use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{RngCore, SeedableRng};

/// Maximum discarded random words for one draw. The rejection probability of
/// a single word is below `2^-32` for any `n` that fits a `u32`, so exhausting
/// this budget is an internal fault rather than a normal result.
const MAX_REJECTIONS: usize = 1024;

/// The permutation of `0..n` produced by an unbiased descending Fisher-Yates
/// pass over the identity, drawing from ChaCha20 seeded with `seed`.
///
/// Applying it as `out[i] = items[permutation[i]]` gives the same order as
/// running that pass directly over `items`. Returns `None` only when a draw
/// exhausts its rejection budget; the result is a pure function of the
/// arguments, so every participant computes the same value.
#[must_use]
pub fn permutation(seed: [u8; 32], n: u32) -> Option<Vec<u32>> {
    let mut rng = ChaCha20Rng::from_seed(seed);
    let mut order: Vec<u32> = (0..n).collect();
    for index in (1..order.len()).rev() {
        let swap = choose(&mut rng, (index + 1) as u64)? as usize;
        order.swap(index, swap);
    }
    Some(order)
}

fn choose(rng: &mut ChaCha20Rng, upper_bound: u64) -> Option<u64> {
    // `wrapping_neg()` computes 2^64 modulo `upper_bound`. Rejecting values
    // below that threshold leaves a whole number of equal-sized residue
    // classes in the remaining 64-bit sample space.
    let threshold = upper_bound.wrapping_neg() % upper_bound;
    for _ in 0..MAX_REJECTIONS {
        let value = rng.next_u64();
        if value >= threshold {
            return Some(value % upper_bound);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permutation_is_a_deterministic_bijection() {
        let first = permutation([7; 32], 64).expect("draws succeed");
        assert_eq!(first, permutation([7; 32], 64).expect("draws succeed"));
        let mut sorted = first.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..64).collect::<Vec<_>>());
        assert_ne!(first, permutation([8; 32], 64).expect("draws succeed"));
        assert_eq!(permutation([7; 32], 0), Some(Vec::new()));
        assert_eq!(permutation([7; 32], 1), Some(vec![0]));
    }
}
