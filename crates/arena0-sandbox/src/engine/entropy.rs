//! Live entropy source for the `random` host function.
//!
//! Determinism requires that randomness be reproducible. This source draws from
//! a seeded ChaCha20 RNG and records every draw so the runtime can expose the
//! exact bytes the guest observed.

use rand::{RngCore, SeedableRng};
use rand_chacha::ChaCha20Rng;

/// Host-owned entropy source threaded through [`HostState`](super::HostState).
pub(crate) struct Entropy {
    rng: ChaCha20Rng,
    /// Exact draws served, awaiting completion.
    log: Vec<Vec<u8>>,
}

// Manual impl: ChaCha20Rng's Debug would leak internal state; summarize instead.
impl std::fmt::Debug for Entropy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entropy")
            .field("recorded", &self.log.len())
            .finish()
    }
}

impl Entropy {
    /// A fresh live source seeded from OS entropy.
    pub(crate) fn live() -> Self {
        Self {
            rng: ChaCha20Rng::from_rng(rand::thread_rng()).expect("thread_rng is infallible"),
            log: Vec::new(),
        }
    }

    /// Fill `buf` from the live stream, recording the draw.
    pub(crate) fn fill(&mut self, buf: &mut [u8]) {
        self.rng.fill_bytes(buf);
        self.log.push(buf.to_vec());
    }

    /// Complete one invocation and return every consumed draw.
    pub(crate) fn finish(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.log)
    }

    /// Start a fresh live entropy stream for the next resident dispatch.
    pub(crate) fn reset(&mut self) {
        *self = Self::live();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_records_every_draw() {
        let mut e = Entropy::live();
        let (mut a, mut b) = ([0u8; 8], [0u8; 4]);
        e.fill(&mut a);
        e.fill(&mut b);
        let draws = e.finish();
        assert_eq!(draws, vec![a.to_vec(), b.to_vec()]);
        // drain leaves the log empty.
        assert!(e.finish().is_empty());
    }
}
