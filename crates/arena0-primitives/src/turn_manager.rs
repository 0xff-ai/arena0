//! Round-robin turn management.

use arena0::prelude::*;
/// Round-robin turn tracker.
///
/// Construct with the canonical, distinct session participant order, then call
/// `advance()` after each turn to rotate to the next participant. The supplied
/// order is authoritative and is serialized as part of shared state.
/// Turn-managed phases are single-writer: implement the program's `writer`
/// function with [`current`](Self::current), so the runtime applies only the
/// turn holder's broadcast next.
#[arena0::primitive]
#[derive(Default)]
pub struct TurnManager {
    participants: Vec<Participant>,
    current: u32,
}

impl TurnManager {
    /// Create a turn manager with the given participant order.
    #[must_use]
    pub fn new(participants: Vec<Participant>) -> Self {
        assert!(
            !participants.is_empty(),
            "turn manager requires at least one participant"
        );
        for (index, participant) in participants.iter().enumerate() {
            assert!(
                !participants[..index].contains(participant),
                "turn manager participants must be distinct"
            );
        }
        Self {
            participants,
            current: 0,
        }
    }

    /// The participant whose turn it is.
    #[must_use]
    pub fn current(&self) -> Participant {
        self.participants
            .get(self.current as usize)
            .copied()
            .expect("turn manager requires at least one participant")
    }

    /// Whether `participant` has the current turn.
    #[must_use]
    pub fn is_turn(&self, participant: Participant) -> bool {
        self.current() == participant
    }

    /// Move to the next participant, wrapping around.
    pub fn advance(&mut self) {
        assert!(
            !self.participants.is_empty(),
            "turn manager requires at least one participant"
        );
        let participant_count = u32::try_from(self.participants.len())
            .expect("turn manager supports at most u32::MAX participants");
        self.current = (self.current + 1) % participant_count;
    }

    /// Return the turn-list index for a participant, if present.
    #[must_use]
    pub fn index_of(&self, participant: Participant) -> Option<usize> {
        self.participants.iter().position(|p| *p == participant)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn participant(n: u8) -> Participant {
        Participant::new(n)
    }

    #[test]
    fn round_robin() {
        let mut tm = TurnManager::new(vec![participant(1), participant(2), participant(3)]);
        assert_eq!(tm.current(), participant(1));
        assert!(tm.is_turn(participant(1)));

        tm.advance();
        assert_eq!(tm.current(), participant(2));

        tm.advance();
        assert_eq!(tm.current(), participant(3));

        tm.advance();
        assert_eq!(tm.current(), participant(1));
    }

    #[test]
    fn participant_lookup() {
        let tm = TurnManager::new(vec![participant(10), participant(20)]);
        assert_eq!(tm.index_of(participant(10)), Some(0));
        assert_eq!(tm.index_of(participant(20)), Some(1));
        assert_eq!(tm.index_of(participant(99)), None);
    }

    #[test]
    #[should_panic(expected = "turn manager requires at least one participant")]
    fn new_rejects_empty_participants() {
        let _ = TurnManager::new(Vec::new());
    }

    #[test]
    #[should_panic(expected = "turn manager participants must be distinct")]
    fn new_rejects_duplicate_participants() {
        let _ = TurnManager::new(vec![participant(1), participant(1)]);
    }

    #[test]
    fn shared_state_encoding_has_a_stable_vector() {
        let manager = TurnManager::new(vec![participant(1), participant(3)]);
        assert_eq!(
            borsh::to_vec(&manager).unwrap(),
            [2, 0, 0, 0, 1, 3, 0, 0, 0, 0]
        );
    }
}
