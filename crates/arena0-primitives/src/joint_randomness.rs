//! Deterministic randomness derived from a completed commit-reveal round.
//!
//! [`shuffle`](crate::joint_randomness::shuffle) composes the existing
//! [`CommitReveal<[u8; 32]>`] state machine.
//! It hashes the revealed contributions in their canonical participant order
//! with an explicit versioned domain. The Host draws the permutation through
//! `arena0::permutation`; this helper applies it without sampling ambient
//! randomness or depending on collection iteration order.
//!
//! `shuffle` returns `None` until commit-reveal completes or if the item count
//! does not fit a `u32`. The Host traps above its profile's permutation limit.

use crate::commit_reveal::CommitReveal;

/// Versioned domain for the derived joint-randomness seed.
///
/// The preimage is the UTF-8 bytes of this domain, followed by the little-endian
/// `u32` contribution count and each 32-byte contribution in participant order.
const DOMAIN: &[u8] = b"arena0/joint-randomness/v1";

/// Derive the shared seed from a completed commit-reveal round.
///
/// `CommitReveal::values` exposes values in canonical participant order. The
/// contribution count is part of the preimage so two ensembles with different
/// sizes cannot share a seed merely because their common prefix matches.
fn seed(protocol: &CommitReveal<[u8; 32]>) -> Option<[u8; 32]> {
    let values = protocol.values()?;
    let mut preimage = Vec::with_capacity(DOMAIN.len() + 4 + values.len() * 32);
    preimage.extend_from_slice(DOMAIN);
    preimage.extend_from_slice(&(values.len() as u32).to_le_bytes());
    for value in values {
        preimage.extend_from_slice(value);
    }
    Some(arena0::hash(&preimage))
}

/// Shuffle `items` with an unbiased descending Fisher-Yates pass.
///
/// The initial order supplied by the caller is part of the deterministic input.
/// Callers that need canonical participant behavior must construct that order
/// explicitly before calling this function.
pub fn shuffle<T>(protocol: &CommitReveal<[u8; 32]>, items: &mut [T]) -> Option<()> {
    let seed = seed(protocol)?;
    let mut order = arena0::permutation(seed, u32::try_from(items.len()).ok()?);
    // Each cycle maps destination to original source. Swapping along that
    // cycle pulls each source into place; identity entries mark visited slots.
    for start in 0..order.len() {
        let mut current = start;
        while order[current] as usize != start {
            let next = order[current] as usize;
            items.swap(current, next);
            order[current] = current as u32;
            current = next;
        }
        order[current] = current as u32;
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commit_reveal::{CommitRevealLocal, Phase};
    use arena0::prelude::Participant;

    const P0: Participant = Participant::new(0);
    const P1: Participant = Participant::new(1);
    const P2: Participant = Participant::new(2);

    fn completed(values: [[u8; 32]; 3]) -> CommitReveal<[u8; 32]> {
        CommitReveal::new_completed(values.into_iter().collect())
    }

    fn replicas_for_delivery_orders(
        values: [[u8; 32]; 3],
    ) -> (CommitReveal<[u8; 32]>, CommitReveal<[u8; 32]>) {
        let mut authors: Vec<_> = (0..3)
            .map(|_| {
                let mut protocol = CommitReveal::default();
                protocol.set_participant_count(3).unwrap();
                protocol
            })
            .collect();
        let mut locals: Vec<CommitRevealLocal<[u8; 32]>> =
            (0..3).map(|_| CommitRevealLocal::default()).collect();
        let salts = [[1; 32], [2; 32], [3; 32]];
        let commits: Vec<_> = authors
            .iter()
            .zip(locals.iter_mut())
            .zip(values)
            .zip(salts)
            .map(|(((protocol, local), value), salt)| {
                protocol.commit_with_salt(local, value, salt).unwrap()
            })
            .collect();

        let participants = [P0, P1, P2];
        for (participant, message) in participants.into_iter().zip(commits.iter()) {
            for author in &mut authors {
                author.handle(participant, message.clone()).unwrap();
            }
        }

        let reveals: Vec<_> = authors
            .iter()
            .zip(locals.iter_mut())
            .map(|(protocol, local)| protocol.take_reveal(local).unwrap())
            .collect();

        let mut ordered = CommitReveal::default();
        let mut reordered = CommitReveal::default();
        ordered.set_participant_count(3).unwrap();
        reordered.set_participant_count(3).unwrap();
        for (participant, message) in participants.into_iter().zip(commits.iter()) {
            ordered.handle(participant, message.clone()).unwrap();
        }
        for (participant, index) in [(P2, 2), (P0, 0), (P1, 1)] {
            reordered
                .handle(participant, commits[index].clone())
                .unwrap();
        }

        for (participant, message) in participants.into_iter().zip(reveals.iter()) {
            ordered.handle(participant, message.clone()).unwrap();
        }
        for (participant, index) in [(P1, 1), (P2, 2), (P0, 0)] {
            reordered
                .handle(participant, reveals[index].clone())
                .unwrap();
        }

        assert_eq!(ordered.phase(), Phase::Complete);
        assert_eq!(reordered.phase(), Phase::Complete);
        (ordered, reordered)
    }

    #[test]
    fn seed_has_a_stable_vector() {
        let protocol = completed([[0x11; 32], [0x22; 32], [0x44; 32]]);
        assert_eq!(
            seed(&protocol),
            Some([
                153, 63, 198, 188, 98, 233, 236, 153, 247, 18, 190, 163, 165, 44, 123, 165, 93,
                249, 34, 96, 37, 126, 21, 67, 62, 242, 172, 93, 159, 174, 17, 96,
            ])
        );
    }

    #[test]
    fn delivery_order_does_not_change_seed() {
        let values = [[0x11; 32], [0x22; 32], [0x44; 32]];
        let (ordered, reordered) = replicas_for_delivery_orders(values);
        assert_eq!(seed(&ordered), seed(&reordered));
    }

    #[test]
    fn every_contribution_changes_the_seed() {
        let baseline = [[0x11; 32], [0x22; 32], [0x44; 32]];
        for index in 0..baseline.len() {
            let mut changed = baseline;
            changed[index][0] ^= 1;
            assert_ne!(seed(&completed(baseline)), seed(&completed(changed)));
        }
    }

    #[test]
    fn shuffle_is_stable() {
        let protocol = completed([[0x11; 32], [0x22; 32], [0x44; 32]]);
        let mut values = [0u8, 1, 2, 3, 4];
        shuffle(&protocol, &mut values).unwrap();
        assert_eq!(values, [4, 0, 3, 2, 1]);
    }

    #[test]
    fn incomplete_round_cannot_seed_or_shuffle() {
        let protocol = CommitReveal::<[u8; 32]>::default();
        assert_eq!(seed(&protocol), None);
        let mut values = [1, 2, 3];
        assert_eq!(shuffle(&protocol, &mut values), None);
    }
}
