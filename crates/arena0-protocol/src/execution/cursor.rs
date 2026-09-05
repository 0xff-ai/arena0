use std::fmt;

use arena0_program::SharedStateBytes;
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::StateHash;
use crate::trace::{CHAIN_START, StepCommitment};

use super::ProtocolError;
/// A local durable version.  It advances once for every accepted commit plan.
/// Retrying an already committed input reproduces the same plan identity and
/// is handled by the store's idempotence check.  It does not create a new
/// version.
#[derive(
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
)]
#[serde(transparent)]
pub struct ExecutionVersion(u64);

impl ExecutionVersion {
    /// The version before the first durable plan.
    pub const ZERO: Self = Self(0);

    /// Construct a version from a persisted value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the persisted value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Advance one durable plan, rejecting overflow.
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

impl From<u64> for ExecutionVersion {
    fn from(value: u64) -> Self {
        Self::new(value)
    }
}

impl From<ExecutionVersion> for u64 {
    fn from(value: ExecutionVersion) -> Self {
        value.get()
    }
}

impl fmt::Display for ExecutionVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// The public consensus cursor.  `next_step` is the next canonical trace
/// position.  `state_hash` and `chain_hash` describe the last certified public
/// state and commitment link.
#[derive(
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
)]
pub struct PublicCursor {
    pub(crate) next_step: u64,
    pub(crate) state_hash: StateHash,
    pub(crate) chain_hash: [u8; 32],
}

impl PublicCursor {
    /// Construct a cursor from its exact certified values.
    #[must_use]
    pub const fn new(next_step: u64, state_hash: StateHash, chain_hash: [u8; 32]) -> Self {
        Self {
            next_step,
            state_hash,
            chain_hash,
        }
    }

    /// Construct the cursor before the first public entry.
    #[must_use]
    pub fn genesis(shared_state: &SharedStateBytes) -> Self {
        Self::new(0, StateHash::of(shared_state.as_bytes()), CHAIN_START)
    }

    /// Return the next public step position.
    #[must_use]
    pub const fn next_step(self) -> u64 {
        self.next_step
    }

    /// Return the last certified shared state hash.
    #[must_use]
    pub const fn state_hash(self) -> StateHash {
        self.state_hash
    }

    /// Return the last certified commitment chain hash.
    #[must_use]
    pub const fn chain_hash(self) -> [u8; 32] {
        self.chain_hash
    }

    pub(crate) fn advance(&self, commitment: &StepCommitment) -> Result<Self, ProtocolError> {
        let expected_step = self
            .next_step
            .checked_add(1)
            .ok_or(ProtocolError::PublicCursorExhausted)?;
        if commitment.step != self.next_step {
            return Err(ProtocolError::StepCoordinateMismatch {
                expected: self.next_step,
                actual: commitment.step,
            });
        }
        if commitment.pre_state != self.state_hash {
            return Err(ProtocolError::PublicPreStateMismatch {
                expected: self.state_hash,
                actual: commitment.pre_state,
            });
        }
        if commitment.link != self.chain_hash {
            return Err(ProtocolError::PublicChainMismatch);
        }
        Ok(Self::new(
            expected_step,
            commitment.post_state,
            commitment.link_hash(),
        ))
    }
}

/// The local private-record cursor.  This coordinate is never included in a
/// public commitment or ensemble certificate.
#[derive(
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Default,
)]
pub struct PrivateCursor {
    pub(crate) next_record: u64,
    pub(crate) last_reaction_position: Option<u64>,
}

impl PrivateCursor {
    /// Construct a private cursor from a persisted next-record value.
    #[must_use]
    pub const fn new(next_record: u64) -> Self {
        Self {
            next_record,
            last_reaction_position: None,
        }
    }

    /// Return the next private record sequence number.
    #[must_use]
    pub const fn next_record(self) -> u64 {
        self.next_record
    }

    /// Return the public position whose automatic local reaction was most
    /// recently committed.
    #[must_use]
    pub const fn last_reaction_position(self) -> Option<u64> {
        self.last_reaction_position
    }

    pub(crate) fn advance(&self) -> Result<Self, ProtocolError> {
        self.next_record
            .checked_add(1)
            .map(|next_record| Self {
                next_record,
                last_reaction_position: self.last_reaction_position,
            })
            .ok_or(ProtocolError::PrivateCursorExhausted)
    }

    pub(crate) fn advance_reaction(&self, public_position: u64) -> Result<Self, ProtocolError> {
        if self.last_reaction_position == Some(public_position) {
            return Err(ProtocolError::PrivateReactionAlreadyCommitted {
                position: public_position,
            });
        }
        let mut next = self.advance()?;
        next.last_reaction_position = Some(public_position);
        Ok(next)
    }
}
