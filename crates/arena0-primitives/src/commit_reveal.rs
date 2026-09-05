//! N-party commit-reveal protocol.
//!
//! Each participant commits a value with a random salt, exchanges commitment
//! hashes, then reveals. The protocol guarantees that no participant can change
//! its choice after seeing another participant's committed value.
//!
//! **Phase shape**: commit-reveal is two ordered collect rounds. Every participant broadcasts exactly one
//! `Commit`, applied in participant-index order, then exactly one `Reveal`.
//!
//! **Handler discipline**:
//! [`handle`](crate::commit_reveal::CommitReveal::handle) is the shared-event
//! side. It runs identically on every node (the sender included) at the same
//! public position, keyed by the authenticated sender, and mutates only the
//! shared-visible fields.
//! [`commit_with_salt`](crate::commit_reveal::CommitReveal::commit_with_salt)
//! and [`take_reveal`](crate::commit_reveal::CommitReveal::take_reveal) are
//! local decision code: they read shared state, stash the secret value and salt
//! in a [`crate::commit_reveal::CommitRevealLocal`] companion value, and produce the message to
//! broadcast; shared state moves only when that message applies.
//!
//! Program usage:
//! ```ignore
//! // local (on_input): stash the secret and broadcast the commit
//! ctx.commitments().commit(value)?.broadcast_via(Message::CommitReveal);
//! // local (on_react): once all commits are in, broadcast the reveal
//! if let Some(reveal) = ctx.commitments().take_reveal() {
//!     reveal.broadcast_via(Message::CommitReveal);
//! }
//! // shared (on_message): apply the round for the authenticated sender
//! ctx.commitments().handle(from, msg)?;
//! if ctx.shared().commitments.is_complete() {
//!     // score, then return Transition::End or Transition::To(...)
//! }
//! ```

use arena0::prelude::*;
use borsh::BorshSerialize;

/// Wire messages for the commit-reveal protocol.
#[arena0::message]
pub enum Message<T> {
    Commit([u8; 32]),
    Reveal { value: T, salt: [u8; 32] },
}

/// Protocol phase, tracked in shared state.
#[arena0::phases]
pub enum Phase {
    #[phase(default, description = "Collecting commitments")]
    Idle,
    #[phase(description = "Commitments exchanged, waiting for reveals")]
    Revealing,
    #[phase(description = "All values revealed", terminal)]
    Complete,
}

/// Participant-local companion state for [`CommitReveal`].
///
/// A program embeds this value in its `Program::Local` DTO. Its fields are
/// serialized at every local fresh-call boundary and never enter the shared
/// state commitment.
#[arena0::local]
#[derive(Clone, Default)]
pub struct CommitRevealLocal<T> {
    #[secret]
    pub value: Option<T>,
    #[secret]
    pub salt: Option<[u8; 32]>,
    pub committed_round: Option<u32>,
    pub revealed_round: Option<u32>,
}

/// N-party commit-reveal shared state.
///
/// Shared-visible fields (`phase`, `round`, `hashes`, `values`) are hashed and
/// mutated only by [`handle`](Self::handle) (and [`reset`](Self::reset)),
/// which every node applies identically. This type contains no participant
/// identity or local secret; pair it with [`CommitRevealLocal`] in the
/// program's local DTO.
#[arena0::primitive(capabilities(Messaging))]
pub struct CommitReveal<T> {
    phase: Phase,
    /// Round counter, bumped by [`reset`](Self::reset) so the local stash can
    /// tell a fresh round from a stale one without a local reset.
    round: u32,
    hashes: Vec<Option<[u8; 32]>>,
    values: Vec<Option<T>>,
}

impl<T> Default for CommitReveal<T> {
    fn default() -> Self {
        Self {
            phase: Phase::default(),
            round: 0,
            hashes: vec![None, None],
            values: vec![None, None],
        }
    }
}

/// Errors from the commit-reveal state machine.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("commit-reveal requires at least two participants")]
    TooFewParticipants,
    #[error("commit-reveal participant count must fit in u8")]
    TooManyParticipants,
    #[error("participant {0} is outside the commit-reveal ensemble")]
    UnknownParticipant(usize),
    #[error("participant count cannot change after the round starts")]
    RoundAlreadyStarted,
    #[error("commit received outside the commit round")]
    UnexpectedCommit,
    #[error("second commit from the same participant in one round")]
    DuplicateCommit,
    #[error("reveal received before commit phase")]
    RevealBeforeCommit,
    #[error("second reveal from the same participant in one round")]
    DuplicateReveal,
    #[error("reveal hash does not match commitment")]
    HashMismatch,
    #[error("local value already committed this round")]
    AlreadyCommitted,
    #[error("commit-reveal can reset only after every value is revealed")]
    ResetBeforeComplete,
}

/// Accessor contract for a program's local DTO.
///
/// Programs with more than one local field implement this trait by returning
/// the `CommitRevealLocal<T>` member used by their commit-reveal instance. The
/// primitive extension then keeps shared protocol state and local stash state
/// separate at the context boundary.
pub trait CommitRevealLocalState<T> {
    /// Borrow this program's local commit-reveal stash.
    fn commit_reveal_local(&self) -> &CommitRevealLocal<T>;

    /// Mutably borrow this program's local commit-reveal stash.
    fn commit_reveal_local_mut(&mut self) -> &mut CommitRevealLocal<T>;
}

impl<T> CommitRevealLocalState<T> for CommitRevealLocal<T> {
    fn commit_reveal_local(&self) -> &CommitRevealLocal<T> {
        self
    }

    fn commit_reveal_local_mut(&mut self) -> &mut CommitRevealLocal<T> {
        self
    }
}

/// Local context-field operations for `CommitReveal` primitives.
///
/// `#[arena0::state]` generates field-named accessors such as
/// `ctx.commit_reveal()`. These operations inspect shared protocol state,
/// update the caller's `Program::Local` stash, and return detached outputs that
/// can be broadcast with `.broadcast()` (or `.broadcast_via(...)`).
pub trait CommitRevealLocalFieldExt<T, Route = RawPrimitiveRoute> {
    /// Whether this node still owes its commit for the current round.
    fn needs_commit(self) -> bool;

    /// Commit a local value with a host-generated salt and return the commit
    /// message to broadcast. Local decision code only.
    fn commit(self, value: T) -> Result<PrimitiveOutput<Message<T>, Route>, Error>;

    /// Commit a local value with an explicit salt.
    ///
    /// Use this form for tests and advanced protocols that need deterministic
    /// salts. Ordinary program code should prefer [`commit`](Self::commit).
    fn commit_with_salt(
        self,
        value: T,
        salt: [u8; 32],
    ) -> Result<PrimitiveOutput<Message<T>, Route>, Error>;

    /// Take the reveal message owed this round, once every commitment is in.
    /// Local decision code; returns `None` until the reveal is due and at most
    /// once per round.
    fn take_reveal(self) -> Option<PrimitiveOutput<Message<T>, Route>>;
}

/// Shared context-field operations for `CommitReveal` primitives.
///
/// Shared handlers apply authenticated messages at the canonical public
/// position. This handle exposes only the shared mutation operation.
pub trait CommitRevealSharedFieldExt<T, Route = RawPrimitiveRoute> {
    /// Apply a commit-reveal message from the authenticated sender. Every node
    /// runs this operation at the same public position.
    fn handle(self, from: Participant, msg: Message<T>) -> Result<(), Error>;
}

impl<Shared, Local, T, Route> CommitRevealLocalFieldExt<T, Route>
    for LocalPrimitiveField<'_, Shared, Local, CommitReveal<T>, Route>
where
    Shared: Primitive,
    Local: CommitRevealLocalState<T>,
    T: BorshSerialize + Clone,
{
    fn needs_commit(mut self) -> bool {
        self.with_shared_local(|cr, local| cr.needs_commit(local.commit_reveal_local()))
    }

    fn commit(mut self, value: T) -> Result<PrimitiveOutput<Message<T>, Route>, Error> {
        let salt = self.random_bytes();
        self.commit_with_salt(value, salt)
    }

    fn commit_with_salt(
        mut self,
        value: T,
        salt: [u8; 32],
    ) -> Result<PrimitiveOutput<Message<T>, Route>, Error> {
        let commit = self.with_shared_local(|cr, local| {
            cr.commit_with_salt(local.commit_reveal_local_mut(), value, salt)
        })?;
        Ok(self.output(commit))
    }

    fn take_reveal(mut self) -> Option<PrimitiveOutput<Message<T>, Route>> {
        let reveal =
            self.with_shared_local(|cr, local| cr.take_reveal(local.commit_reveal_local_mut()))?;
        Some(self.output(reveal))
    }
}

impl<Shared, T, Route> CommitRevealSharedFieldExt<T, Route>
    for SharedPrimitiveField<'_, Shared, CommitReveal<T>, Route>
where
    Shared: Primitive,
    T: BorshSerialize + Clone,
{
    fn handle(mut self, from: Participant, msg: Message<T>) -> Result<(), Error> {
        self.mutate(|cr| cr.handle(from, msg))
    }
}

impl<T> CommitReveal<T> {
    /// Configure the participant count before the first commit.
    ///
    /// Bilateral programs can keep using [`Default`], which configures two
    /// participants. N-party programs call this from their session-start
    /// handler with the committed ensemble size.
    pub fn set_participant_count(&mut self, participant_count: usize) -> Result<(), Error> {
        if participant_count < 2 {
            return Err(Error::TooFewParticipants);
        }
        if u8::try_from(participant_count).is_err() {
            return Err(Error::TooManyParticipants);
        }
        if self.phase != Phase::Idle || self.hashes.iter().any(Option::is_some) {
            return Err(Error::RoundAlreadyStarted);
        }
        self.hashes = vec![None; participant_count];
        self.values = (0..participant_count).map(|_| None).collect();
        Ok(())
    }

    /// Number of participants configured for this protocol.
    #[must_use]
    pub fn participant_count(&self) -> usize {
        self.hashes.len()
    }

    /// The participant whose message the current base is waiting for, if any:
    /// the first missing slot in the current phase (commit hashes during
    /// `Idle`, revealed values during `Revealing`). The unique-writer rule: a
    /// message from any other participant is a deterministic reject.
    #[must_use]
    pub fn expected_writer(&self) -> Option<Participant> {
        let slot = match self.phase {
            Phase::Idle => self.hashes.iter().position(Option::is_none),
            Phase::Revealing => self.values.iter().position(Option::is_none),
            Phase::Complete => None,
        };
        slot.and_then(|index| Participant::try_from(index).ok())
    }
    /// Reset for a new round. Shared-handler code: bumps the shared round
    /// counter so each node's local stash keys itself to the fresh round.
    pub fn reset(&mut self) -> Result<(), Error> {
        if self.phase != Phase::Complete {
            return Err(Error::ResetBeforeComplete);
        }
        self.phase = Phase::default();
        self.round += 1;
        self.hashes.fill(None);
        for value in &mut self.values {
            *value = None;
        }
        Ok(())
    }

    /// Current protocol phase.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Current round number (bumped by [`reset`](Self::reset)).
    pub fn round(&self) -> u32 {
        self.round
    }

    /// Whether all values have been revealed.
    pub fn is_complete(&self) -> bool {
        self.phase == Phase::Complete
    }

    /// Whether the supplied local stash still owes its commit for the current
    /// round.
    ///
    /// Local decision code should normally call the matching method on its
    /// `CommitRevealLocalFieldExt` handle, which supplies the program-local DTO.
    pub fn needs_commit(&self, local: &CommitRevealLocal<T>) -> bool {
        self.phase == Phase::Idle && local.committed_round != Some(self.round)
    }

    /// Returns the revealed value at the given participant, if complete.
    pub fn value_at(&self, participant: usize) -> Option<&T> {
        (self.phase() == Phase::Complete).then(|| self.values.get(participant)?.as_ref())?
    }

    /// Returns all revealed values in participant order.
    pub fn values(&self) -> Option<Vec<&T>> {
        if self.phase == Phase::Complete {
            self.values.iter().map(Option::as_ref).collect()
        } else {
            None
        }
    }

    /// Stash a value and salt in the supplied local DTO and return the Commit
    /// message to broadcast. Local decision code: does NOT mutate
    /// shared-visible state; this node's hash lands in shared state when its
    /// own broadcast applies through [`handle`](Self::handle).
    pub fn commit_with_salt(
        &self,
        local: &mut CommitRevealLocal<T>,
        value: T,
        salt: [u8; 32],
    ) -> Result<Message<T>, Error>
    where
        T: BorshSerialize,
    {
        if local.committed_round == Some(self.round) {
            return Err(Error::AlreadyCommitted);
        }
        let hash = compute_hash(&value, &salt);
        local.value = Some(value);
        local.salt = Some(salt);
        local.committed_round = Some(self.round);
        Ok(Message::Commit(hash))
    }

    /// Apply a message from the authenticated sender. The shared-event side:
    /// pure over shared state plus the event, identical on every node.
    ///
    /// A `Commit` outside the commit round, a second message from the same
    /// participant inside a round, or a reveal that fails its commitment hash
    /// is a protocol violation surfaced as `Err` (the program's shared handler
    /// propagates it, aborting identically on every node).
    pub fn handle(&mut self, from: Participant, msg: Message<T>) -> Result<(), Error>
    where
        T: BorshSerialize,
    {
        let slot = from.index();
        if slot >= self.hashes.len() {
            return Err(Error::UnknownParticipant(slot));
        }
        match msg {
            Message::Commit(hash) => {
                if self.phase != Phase::Idle {
                    return Err(Error::UnexpectedCommit);
                }
                if self.hashes[slot].is_some() {
                    return Err(Error::DuplicateCommit);
                }
                self.hashes[slot] = Some(hash);
                if self.hashes.iter().all(Option::is_some) {
                    self.phase = Phase::Revealing;
                }
                Ok(())
            }
            Message::Reveal { value, salt } => {
                if self.phase != Phase::Revealing {
                    return Err(Error::RevealBeforeCommit);
                }
                if self.values[slot].is_some() {
                    return Err(Error::DuplicateReveal);
                }
                let committed = self.hashes[slot].ok_or(Error::RevealBeforeCommit)?;
                if compute_hash(&value, &salt) != committed {
                    return Err(Error::HashMismatch);
                }
                self.values[slot] = Some(value);
                if self.values.iter().all(Option::is_some) {
                    self.phase = Phase::Complete;
                }
                Ok(())
            }
        }
    }

    /// Take the reveal owed for the current round: `Some` exactly once, when
    /// every commitment has been applied and this node committed this round.
    /// Local decision code (reads and writes the supplied local stash only).
    pub fn take_reveal(&self, local: &mut CommitRevealLocal<T>) -> Option<Message<T>>
    where
        T: Clone,
    {
        if self.phase != Phase::Revealing
            || local.committed_round != Some(self.round)
            || local.revealed_round == Some(self.round)
        {
            return None;
        }
        let value = local.value.clone()?;
        let salt = local.salt?;
        local.revealed_round = Some(self.round);
        Some(Message::Reveal { value, salt })
    }
}

/// Randomness extraction for commit-reveal of 32-byte nonces.
impl CommitReveal<[u8; 32]> {
    /// XOR of all participant nonces when complete.
    #[must_use]
    pub fn random_bytes(&self) -> Option<[u8; 32]> {
        let values = self.values()?;
        let mut result = [0u8; 32];
        for value in values {
            for i in 0..32 {
                result[i] ^= value[i];
            }
        }
        Some(result)
    }

    /// First 8 bytes of the shared random value as a little-endian u64.
    #[must_use]
    pub fn random_u64(&self) -> Option<u64> {
        let bytes = self.random_bytes()?;
        let arr: [u8; 8] = bytes[..8].try_into().expect("slice is exactly 8 bytes");
        Some(u64::from_le_bytes(arr))
    }

    /// Random u64 modulo `max`, giving a value in `0..max`.
    #[must_use]
    pub fn random_range(&self, max: u64) -> Option<u64> {
        self.random_u64().map(|v| v % max)
    }

    /// First bit of the shared random value.
    #[must_use]
    pub fn coin_flip(&self) -> Option<bool> {
        self.random_bytes().map(|b| b[0] & 1 == 1)
    }
}

/// Compute the commitment used by the existing arena0 commit-reveal contract.
///
/// The exact preimage is stock Borsh serialization of the tuple `(value,
/// salt)`, in that order. The commitment is the 32-byte BLAKE3 digest of those
/// bytes with no additional prefix or context. This legacy domain is pinned so
/// every existing receipt remains replayable; new protocols that need another
/// domain must define a separate construction rather than silently changing
/// this one.
pub(crate) fn compute_hash<T: BorshSerialize>(value: &T, salt: &[u8; 32]) -> [u8; 32] {
    let bytes = borsh::to_vec(&(value, salt)).expect("borsh serialization failed");
    *blake3::hash(&bytes).as_bytes()
}

#[cfg(test)]
impl<T> CommitReveal<T> {
    /// Create a completed state for testing randomness extraction.
    pub(crate) fn new_completed(values: Vec<T>) -> Self {
        Self {
            phase: Phase::Complete,
            hashes: vec![Some([0; 32]); values.len()],
            values: values.into_iter().map(Some).collect(),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P0: Participant = Participant::new(0);
    const P1: Participant = Participant::new(1);
    const P2: Participant = Participant::new(2);

    /// Apply the same shared event to both replicas, as the runtime does at
    /// one canonical position, and assert the shared bytes stay identical.
    fn apply_both(
        a: &mut CommitReveal<u32>,
        b: &mut CommitReveal<u32>,
        from: Participant,
        msg: &Message<u32>,
    ) {
        a.handle(from, msg.clone()).unwrap();
        b.handle(from, msg.clone()).unwrap();
        let a_buf = borsh::to_vec(a).unwrap();
        let b_buf = borsh::to_vec(b).unwrap();
        assert_eq!(a_buf, b_buf, "shared must match after a symmetric apply");
    }

    #[test]
    fn symmetric_round_converges() {
        let mut a = CommitReveal::<u32>::default();
        let mut b = CommitReveal::<u32>::default();
        let mut a_local = CommitRevealLocal::default();
        let mut b_local = CommitRevealLocal::default();

        // Local decision code on each node: stash and produce the commit.
        let a_commit = a.commit_with_salt(&mut a_local, 42, [0xAA; 32]).unwrap();
        let b_commit = b.commit_with_salt(&mut b_local, 99, [0xBB; 32]).unwrap();

        // Canonical order: P0's commit then P1's, applied by both replicas.
        apply_both(&mut a, &mut b, P0, &a_commit);
        assert_eq!(a.phase(), Phase::Idle);
        apply_both(&mut a, &mut b, P1, &b_commit);
        assert_eq!(a.phase(), Phase::Revealing);

        // Reveals become due on both nodes, exactly once.
        let a_reveal = a.take_reveal(&mut a_local).expect("A owes a reveal");
        let b_reveal = b.take_reveal(&mut b_local).expect("B owes a reveal");
        assert!(
            a.take_reveal(&mut a_local).is_none(),
            "reveal is taken at most once"
        );

        apply_both(&mut a, &mut b, P0, &a_reveal);
        apply_both(&mut a, &mut b, P1, &b_reveal);
        assert_eq!(a.phase(), Phase::Complete);
        assert_eq!(b.phase(), Phase::Complete);

        let vals = a.values().unwrap();
        assert_eq!((*vals[0], *vals[1]), (42, 99));
    }

    #[test]
    fn three_party_round_converges_and_combines_randomness() {
        let mut replicas: Vec<CommitReveal<[u8; 32]>> = (0..3)
            .map(|_| {
                let mut protocol = CommitReveal::default();
                protocol.set_participant_count(3).unwrap();
                protocol
            })
            .collect();
        let mut locals: Vec<CommitRevealLocal<[u8; 32]>> =
            (0..3).map(|_| CommitRevealLocal::default()).collect();
        let values = [[0x11; 32], [0x22; 32], [0x44; 32]];
        let commits: Vec<_> = replicas
            .iter_mut()
            .zip(locals.iter_mut())
            .zip(values)
            .enumerate()
            .map(|(i, ((protocol, local), value))| {
                protocol
                    .commit_with_salt(local, value, [i as u8; 32])
                    .unwrap()
            })
            .collect();

        for (participant, commit) in [P0, P1, P2].into_iter().zip(&commits) {
            for replica in &mut replicas {
                replica.handle(participant, commit.clone()).unwrap();
            }
        }
        assert!(
            replicas
                .iter()
                .all(|protocol| protocol.phase() == Phase::Revealing)
        );

        let reveals: Vec<_> = replicas
            .iter_mut()
            .zip(locals.iter_mut())
            .map(|(protocol, local)| protocol.take_reveal(local).unwrap())
            .collect();
        for (participant, reveal) in [P0, P1, P2].into_iter().zip(&reveals) {
            for replica in &mut replicas {
                replica.handle(participant, reveal.clone()).unwrap();
            }
        }

        assert!(replicas.iter().all(CommitReveal::is_complete));
        assert!(
            replicas
                .iter()
                .all(|protocol| protocol.random_bytes() == Some([0x77; 32]))
        );
    }

    #[test]
    fn reset_starts_a_fresh_round() {
        let mut a = CommitReveal::<u32>::default();
        let mut b = CommitReveal::<u32>::default();
        let mut a_local = CommitRevealLocal::default();
        let mut b_local = CommitRevealLocal::default();
        let a_commit = a.commit_with_salt(&mut a_local, 1, [1; 32]).unwrap();
        let b_commit = b.commit_with_salt(&mut b_local, 2, [2; 32]).unwrap();
        apply_both(&mut a, &mut b, P0, &a_commit);
        apply_both(&mut a, &mut b, P1, &b_commit);
        let a_reveal = a.take_reveal(&mut a_local).unwrap();
        let b_reveal = b.take_reveal(&mut b_local).unwrap();
        apply_both(&mut a, &mut b, P0, &a_reveal);
        apply_both(&mut a, &mut b, P1, &b_reveal);
        assert!(a.is_complete());

        // Shared-handler code resets the round on every node.
        a.reset().unwrap();
        b.reset().unwrap();
        assert_eq!(a.round(), 1);
        assert!(a.needs_commit(&a_local));
        assert!(
            a.take_reveal(&mut a_local).is_none(),
            "stale stash never re-reveals"
        );

        let a_commit = a.commit_with_salt(&mut a_local, 7, [7; 32]).unwrap();
        assert!(!a.needs_commit(&a_local));
        let b_commit = b.commit_with_salt(&mut b_local, 8, [8; 32]).unwrap();
        apply_both(&mut a, &mut b, P0, &a_commit);
        apply_both(&mut a, &mut b, P1, &b_commit);
        assert_eq!(a.phase(), Phase::Revealing);
    }

    #[test]
    fn duplicate_commit_is_a_protocol_violation() {
        let mut a = CommitReveal::<u32>::default();
        a.handle(P0, Message::Commit([1; 32])).unwrap();
        let err = a.handle(P0, Message::Commit([2; 32]));
        assert!(matches!(err, Err(Error::DuplicateCommit)));
    }

    #[test]
    fn participant_configuration_is_bounded_and_sender_checked() {
        let mut protocol = CommitReveal::<u32>::default();
        assert!(matches!(
            protocol.set_participant_count(1),
            Err(Error::TooFewParticipants)
        ));
        assert!(matches!(
            protocol.set_participant_count(usize::from(u8::MAX) + 1),
            Err(Error::TooManyParticipants)
        ));
        assert!(matches!(
            protocol.handle(P2, Message::Commit([0; 32])),
            Err(Error::UnknownParticipant(2))
        ));
    }

    #[test]
    fn double_local_commit_is_rejected() {
        let a = CommitReveal::<u32>::default();
        let mut local = CommitRevealLocal::default();
        a.commit_with_salt(&mut local, 1, [1; 32]).unwrap();
        assert!(matches!(
            a.commit_with_salt(&mut local, 2, [2; 32]),
            Err(Error::AlreadyCommitted)
        ));
    }

    #[test]
    fn hash_mismatch_returns_error() {
        let mut a = CommitReveal::<u32>::default();
        let commit = compute_hash(&42u32, &[0xAA; 32]);
        a.handle(P0, Message::Commit(commit)).unwrap();
        a.handle(P1, Message::Commit(compute_hash(&9u32, &[0xBB; 32])))
            .unwrap();
        assert_eq!(a.phase(), Phase::Revealing);

        let err = a.handle(
            P0,
            Message::Reveal {
                value: 123u32,
                salt: [0xAA; 32],
            },
        );
        assert!(matches!(err, Err(Error::HashMismatch)));
    }

    #[test]
    fn commitment_domain_has_a_stable_vector() {
        assert_eq!(
            compute_hash(&42_u32, &[0xaa; 32]),
            [
                166, 177, 202, 249, 4, 19, 11, 120, 61, 238, 234, 177, 23, 235, 135, 42, 201, 131,
                1, 184, 174, 10, 92, 21, 159, 121, 61, 235, 253, 187, 241, 27,
            ]
        );
    }

    #[test]
    fn duplicate_reveal_is_a_protocol_violation() {
        let mut protocol = CommitReveal::<u32>::default();
        let mut p0 = CommitRevealLocal::default();
        let mut p1 = CommitRevealLocal::default();
        let p0_commit = protocol.commit_with_salt(&mut p0, 10, [1; 32]).unwrap();
        let p1_commit = protocol.commit_with_salt(&mut p1, 20, [2; 32]).unwrap();
        protocol.handle(P0, p0_commit).unwrap();
        protocol.handle(P1, p1_commit).unwrap();
        let reveal = protocol.take_reveal(&mut p0).unwrap();
        protocol.handle(P0, reveal.clone()).unwrap();
        assert!(matches!(
            protocol.handle(P0, reveal),
            Err(Error::DuplicateReveal)
        ));
    }

    #[test]
    fn a_missing_reveal_keeps_the_round_pending() {
        let mut protocol = CommitReveal::<u32>::default();
        let mut p0 = CommitRevealLocal::default();
        let mut p1 = CommitRevealLocal::default();
        let p0_commit = protocol.commit_with_salt(&mut p0, 10, [1; 32]).unwrap();
        let p1_commit = protocol.commit_with_salt(&mut p1, 20, [2; 32]).unwrap();
        protocol.handle(P0, p0_commit).unwrap();
        protocol.handle(P1, p1_commit).unwrap();
        let reveal = protocol.take_reveal(&mut p0).unwrap();
        protocol.handle(P0, reveal).unwrap();

        assert_eq!(protocol.phase(), Phase::Revealing);
        assert_eq!(protocol.expected_writer(), Some(P1));
        assert!(!protocol.is_complete());
    }

    #[test]
    fn reset_rejects_an_incomplete_round() {
        let mut protocol = CommitReveal::<u32>::default();
        assert!(matches!(protocol.reset(), Err(Error::ResetBeforeComplete)));
    }

    #[test]
    fn reveal_before_commit_returns_error() {
        let mut a = CommitReveal::<u32>::default();
        let err = a.handle(
            P0,
            Message::Reveal {
                value: 42u32,
                salt: [0xAA; 32],
            },
        );
        assert!(matches!(err, Err(Error::RevealBeforeCommit)));
    }

    #[test]
    fn value_at_returns_none_before_completion() {
        let p = CommitReveal::<u32>::default();
        assert!(p.value_at(0).is_none());
        assert!(p.values().is_none());
    }

    #[test]
    fn completed_state_roundtrip() {
        let cr = CommitReveal::new_completed(vec![5u32, 9u32]);
        let buf = borsh::to_vec(&cr).unwrap();
        let restored: CommitReveal<u32> = borsh::from_slice(&buf).unwrap();
        let vals = restored.values().unwrap();
        assert_eq!((*vals[0], *vals[1]), (5, 9));
    }

    #[test]
    fn restored_shared_state_accepts_the_pending_local_reveal() {
        let mut shared = CommitReveal::<u32>::default();
        let mut local = CommitRevealLocal::default();
        let own = shared.commit_with_salt(&mut local, 42, [0xAA; 32]).unwrap();
        let mut peer_local = CommitRevealLocal::default();
        let other = shared
            .commit_with_salt(&mut peer_local, 99, [0xBB; 32])
            .unwrap();
        shared.handle(P0, own).unwrap();
        shared.handle(P1, other).unwrap();

        let bytes = borsh::to_vec(&shared).unwrap();
        let mut restored: CommitReveal<u32> = borsh::from_slice(&bytes).unwrap();
        let reveal = restored.take_reveal(&mut local).unwrap();
        assert!(restored.take_reveal(&mut local).is_none());
        restored.handle(P0, reveal).unwrap();
        let peer_reveal = restored.take_reveal(&mut peer_local).unwrap();
        restored.handle(P1, peer_reveal).unwrap();
        let values = restored.values().unwrap();
        assert_eq!((*values[0], *values[1]), (42, 99));
    }
}
