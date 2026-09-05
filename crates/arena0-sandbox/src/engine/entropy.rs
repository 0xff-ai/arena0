//! Recorded, replayable entropy source for the `random` host function.
//!
//! Determinism requires that randomness be reproducible. In live mode this
//! source draws from a seeded ChaCha20 RNG and records every draw so the runtime
//! can persist it as a replayable runtime-visibility event. In replay mode it
//! serves previously recorded draws in order. Recording the *outputs* (not the
//! seed) is what makes the seed irrelevant to replay.

use std::collections::VecDeque;

use rand::{RngCore, SeedableRng};
use rand_chacha::ChaCha20Rng;

/// Host-owned entropy source threaded through [`HostState`](super::HostState).
pub(crate) struct Entropy {
    rng: ChaCha20Rng,
    /// `Some` => replay mode: serve these recorded draws in order.
    replay: Option<VecDeque<Vec<u8>>>,
    /// Exact draws served in either live or replay mode, awaiting completion.
    log: Vec<Vec<u8>>,
    /// Total draws served by this invocation.
    draws: u64,
}

// Manual impl: ChaCha20Rng's Debug would leak internal state; summarize instead.
impl std::fmt::Debug for Entropy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entropy")
            .field(
                "mode",
                &if self.replay.is_some() {
                    "replay"
                } else {
                    "live"
                },
            )
            .field("recorded", &self.log.len())
            .finish()
    }
}

impl Entropy {
    /// A fresh live source seeded from OS entropy.
    pub(crate) fn live() -> Self {
        Self {
            rng: ChaCha20Rng::from_rng(rand::thread_rng()).expect("thread_rng is infallible"),
            replay: None,
            log: Vec::new(),
            draws: 0,
        }
    }

    /// Fill `buf`, recording the draw (live) or serving a recorded one (replay).
    pub(crate) fn fill(&mut self, buf: &mut [u8]) -> Result<(), wasmtime::Error> {
        self.draws += 1;
        if let Some(queue) = self.replay.as_mut() {
            let bytes = queue
                .pop_front()
                .ok_or_else(|| wasmtime::Error::msg("random: replay underrun"))?;
            if bytes.len() != buf.len() {
                return Err(wasmtime::Error::msg(format!(
                    "random: replay length mismatch (recorded {}, requested {})",
                    bytes.len(),
                    buf.len()
                )));
            }
            buf.copy_from_slice(&bytes);
            self.log.push(bytes);
        } else {
            self.rng.fill_bytes(buf);
            self.log.push(buf.to_vec());
        }
        Ok(())
    }

    /// Complete one invocation and return every consumed draw. Replay succeeds
    /// only when the guest consumed the supplied sequence exactly.
    pub(crate) fn finish(&mut self) -> Result<Vec<Vec<u8>>, wasmtime::Error> {
        if let Some(queue) = &self.replay
            && !queue.is_empty()
        {
            return Err(wasmtime::Error::msg(format!(
                "random: replay has {} unconsumed draws",
                queue.len()
            )));
        }
        Ok(std::mem::take(&mut self.log))
    }

    /// Switch to replay mode, serving `draws` in order to subsequent fills.
    pub(crate) fn set_replay(&mut self, draws: Vec<Vec<u8>>) {
        self.replay = Some(draws.into());
        self.log.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_records_every_draw() {
        let mut e = Entropy::live();
        let (mut a, mut b) = ([0u8; 8], [0u8; 4]);
        e.fill(&mut a).unwrap();
        e.fill(&mut b).unwrap();
        let draws = e.finish().unwrap();
        assert_eq!(draws, vec![a.to_vec(), b.to_vec()]);
        // drain leaves the log empty.
        assert!(e.finish().unwrap().is_empty());
    }

    #[test]
    fn replay_reproduces_recorded_draws_in_order() {
        let mut live = Entropy::live();
        let (mut x, mut y) = ([0u8; 16], [0u8; 3]);
        live.fill(&mut x).unwrap();
        live.fill(&mut y).unwrap();
        let recorded = live.finish().unwrap();

        let mut replay = Entropy::live();
        replay.set_replay(recorded.clone());
        let (mut x2, mut y2) = ([0u8; 16], [0u8; 3]);
        replay.fill(&mut x2).unwrap();
        replay.fill(&mut y2).unwrap();
        assert_eq!(x2.to_vec(), recorded[0]);
        assert_eq!(y2.to_vec(), recorded[1]);
        assert_eq!(replay.finish().unwrap(), recorded);
    }

    #[test]
    fn replay_length_mismatch_traps() {
        let mut e = Entropy::live();
        e.set_replay(vec![vec![1, 2, 3]]);
        let mut buf = [0u8; 4];
        assert!(e.fill(&mut buf).is_err());
    }

    #[test]
    fn replay_underrun_traps() {
        let mut e = Entropy::live();
        e.set_replay(Vec::new());
        let mut buf = [0u8; 1];
        assert!(e.fill(&mut buf).is_err());
    }

    #[test]
    fn replay_rejects_unconsumed_draws() {
        let mut e = Entropy::live();
        e.set_replay(vec![vec![1], vec![2]]);
        let mut buf = [0u8; 1];
        e.fill(&mut buf).unwrap();
        assert!(e.finish().is_err());
    }
}
