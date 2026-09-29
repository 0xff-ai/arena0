use std::fmt;

use arena0_program::SharedStateBytes;
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::StateHash;
use crate::trace::{CHAIN_START, StepCommitment};

use super::ProtocolError;

/// A local durable version, distinct from event positions and agreed steps.
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
    /// The version before the first durable action.
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

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

/// The agreed-step coordinate. It describes the next step to be agreed,
/// together with the last agreed shared state and commitment link.
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
pub struct StepCursor {
    next_step: u64,
    state_hash: StateHash,
    chain_hash: [u8; 32],
}

impl StepCursor {
    /// Construct a cursor from exact agreed values.
    #[must_use]
    pub const fn new(next_step: u64, state_hash: StateHash, chain_hash: [u8; 32]) -> Self {
        Self {
            next_step,
            state_hash,
            chain_hash,
        }
    }

    /// Construct the coordinate before the first agreed step.
    #[must_use]
    pub fn genesis(shared_state: &SharedStateBytes) -> Self {
        Self::new(0, StateHash::of_shared(shared_state), CHAIN_START)
    }

    #[must_use]
    pub const fn next_step(self) -> u64 {
        self.next_step
    }

    #[must_use]
    pub const fn state_hash(self) -> StateHash {
        self.state_hash
    }

    #[must_use]
    pub const fn chain_hash(self) -> [u8; 32] {
        self.chain_hash
    }

    pub(crate) fn advance(&self, commitment: &StepCommitment) -> Result<Self, ProtocolError> {
        let next_step = self
            .next_step
            .checked_add(1)
            .ok_or(ProtocolError::AgreedStepExhausted)?;
        if commitment.step != self.next_step {
            return Err(ProtocolError::StepCoordinateMismatch {
                expected: self.next_step,
                actual: commitment.step,
            });
        }
        if commitment.pre_state != self.state_hash {
            return Err(ProtocolError::AgreedPreStateMismatch {
                expected: self.state_hash,
                actual: commitment.pre_state,
            });
        }
        if commitment.link != self.chain_hash {
            return Err(ProtocolError::AgreedChainMismatch);
        }
        Ok(Self::new(
            next_step,
            commitment.post_state,
            commitment.link_hash(),
        ))
    }
}
