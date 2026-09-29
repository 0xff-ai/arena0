//! The single durable owner of execution progress and terminal evidence.
//!
//! [`ExecLifecycle`](crate::ExecLifecycle) is derived from [`ExecutionStatus`]
//! for public projections. The persisted aggregate stores this enum only; it
//! does not maintain independent lifecycle, pending, proof, or terminal-cause
//! fields that could drift into an invalid cross-product.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::TraceEntry;
use crate::exec::ExecLifecycle;
use crate::trace::StepCommitment;
use arena0_program::bounded;

use super::{
    AbortKind, AbortOccurrence, ExecutionBinding, MAX_TERMINAL_REASON_BYTES, ProtocolError,
    ReceiptId, StepCursor, TerminalOutcome, ensure_payload,
};

/// The one persisted owner of execution lifecycle progress,
/// certified terminal outcomes, and receipt identities.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ExecutionStatus {
    /// Activation is committed but the host has not injected SessionStarted.
    Activating,
    /// The execution is runnable. Any open callout is stored separately with
    /// the resulting state image, not inside this status.
    Active,
    /// The final SessionEnd step is certified and awaits receipt publication.
    Certified { outcome: TerminalOutcome },
    /// Completion evidence has been published.
    Completed {
        outcome: TerminalOutcome,
        receipt_id: ReceiptId,
    },
    /// An authenticated or shared stop awaits local artifact publication.
    Stopped { cause: StopCause },
    /// A canonical shared-stop receipt or unilateral report was published.
    StoppedPublished {
        cause: StopCause,

        receipt_id: ReceiptId,
    },
}

/// Remaining terminal work derived from the authoritative durable status.
/// This view is never persisted and does not permit further execution after certification.
#[derive(Debug, Clone, Copy)]
pub enum ReceiptWork {
    /// Execution has not reached a terminal boundary.
    NotTerminal,
    /// Complete terminal evidence can be assembled into a local receipt body.
    Assemble,
    /// A locally produced artifact has already been persisted.
    Published,
}

/// The one terminal-cause type used by [`ExecutionStatus`]. Local versus peer
/// authorship is derived from an authenticated occurrence's sender and the
/// execution producer; it is not a second persisted status dimension.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub enum StopCause {
    /// A signed local or peer abort/failure occurrence.
    Authenticated(AbortOccurrence),
    /// A certified guest-authored terminal effect.
    Shared {
        kind: AbortKind,
        commitment: StepCommitment,
        #[borsh(
            serialize_with = "bounded::write_string::<MAX_TERMINAL_REASON_BYTES>",
            deserialize_with = "bounded::read_string::<MAX_TERMINAL_REASON_BYTES>"
        )]
        reason: String,
    },
}

impl StopCause {
    /// Borrow the authenticated or shared terminal reason.
    #[must_use]
    pub fn reason(&self) -> &str {
        match self {
            Self::Authenticated(occurrence) => occurrence.reason(),
            Self::Shared { reason, .. } => reason,
        }
    }

    /// The step this stop concludes: the next step of an authenticated
    /// occurrence's cursor, or the certified shared step that stopped.
    #[must_use]
    pub const fn step(&self) -> u64 {
        match self {
            Self::Authenticated(occurrence) => occurrence.coordinate().next_step(),
            Self::Shared { commitment, .. } => commitment.step,
        }
    }

    /// Return the terminal kind when this cause is an abort/failure cause.
    #[must_use]
    pub const fn kind(&self) -> AbortKind {
        match self {
            Self::Authenticated(occurrence) => occurrence.kind(),
            Self::Shared { kind, .. } => *kind,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Authenticated(occurrence) => occurrence.validate_shape(),
            Self::Shared {
                commitment, reason, ..
            } => {
                if !commitment.has_step_domain() {
                    return Err(ProtocolError::InvalidTerminalStatus);
                }
                ensure_reason(reason)
            }
        }
    }
}

impl ExecutionStatus {
    /// Remaining local assembly and publication work.
    #[must_use]
    pub const fn receipt_work(&self) -> ReceiptWork {
        match self {
            Self::Activating | Self::Active => ReceiptWork::NotTerminal,
            Self::Certified { .. } | Self::Stopped { .. } => ReceiptWork::Assemble,
            Self::Completed { .. } | Self::StoppedPublished { .. } => ReceiptWork::Published,
        }
    }

    /// Public lifecycle; completion is observed at receipt publication.
    #[must_use]
    pub const fn lifecycle(&self) -> ExecLifecycle {
        match self {
            Self::Activating => ExecLifecycle::Activating,
            Self::Active | Self::Certified { .. } => ExecLifecycle::Active,
            Self::Completed { .. } => ExecLifecycle::Completed,
            Self::Stopped { cause } | Self::StoppedPublished { cause, .. } => match cause.kind() {
                AbortKind::Fail => ExecLifecycle::Failed,
                AbortKind::Abort => ExecLifecycle::Aborted,
            },
        }
    }

    /// Whether this status can dispatch another input.
    #[must_use]
    pub const fn is_runnable(&self) -> bool {
        matches!(self, Self::Active)
    }

    /// Whether execution has ended and cannot accept further inputs.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        !matches!(self, Self::Activating | Self::Active)
    }

    /// Borrow the authenticated or shared stopping cause.
    #[must_use]
    pub const fn terminal_cause(&self) -> Option<&StopCause> {
        match self {
            Self::Stopped { cause } | Self::StoppedPublished { cause, .. } => Some(cause),
            _ => None,
        }
    }

    pub const fn active() -> Self {
        Self::Active
    }

    pub fn completed(outcome: TerminalOutcome, receipt_id: ReceiptId) -> Self {
        Self::Completed {
            outcome,
            receipt_id,
        }
    }

    pub fn stopped_published(cause: StopCause, receipt_id: ReceiptId) -> Self {
        Self::StoppedPublished { cause, receipt_id }
    }

    /// Construct a status from an authenticated abort/failure occurrence.
    pub fn stopped(occurrence: AbortOccurrence) -> Result<Self, ProtocolError> {
        occurrence.validate_shape()?;
        Ok(Self::Stopped {
            cause: StopCause::Authenticated(occurrence),
        })
    }

    /// Construct the stopping status represented by a certified shared abort/failure.
    pub fn from_shared_entry(
        entry: &TraceEntry,
        commitment: StepCommitment,
    ) -> Result<Option<Self>, ProtocolError> {
        let Some(terminal) = entry.terminal.as_ref() else {
            return Ok(None);
        };
        let Some(reason) = terminal.abort_reason() else {
            return Ok(None);
        };
        ensure_reason(reason)?;
        let kind = match terminal {
            crate::StepTerminal::Abort { .. } => AbortKind::Abort,
            crate::StepTerminal::Fail { .. } => AbortKind::Fail,
            crate::StepTerminal::End { .. } => return Ok(None),
        };
        Ok(Some(Self::Stopped {
            cause: StopCause::Shared {
                kind,
                commitment,
                reason: reason.to_owned(),
            },
        }))
    }

    /// Validate status against the activation binding and agreed cursor.
    pub(crate) fn validate_binding(
        &self,
        binding: &ExecutionBinding,
        agreed: StepCursor,
    ) -> Result<(), ProtocolError> {
        match self {
            Self::Activating | Self::Active => Ok(()),
            Self::Certified { outcome } | Self::Completed { outcome, .. } => {
                outcome.validate()?;
                if agreed.next_step() == 0 {
                    return Err(ProtocolError::InvalidTerminalStatus);
                }
                if matches!(self, Self::Completed { receipt_id, .. } if *receipt_id == ReceiptId::from_bytes([0; 32]))
                {
                    return Err(ProtocolError::InvalidTerminalStatus);
                }
                Ok(())
            }
            Self::Stopped { cause } | Self::StoppedPublished { cause, .. } => {
                cause.validate()?;
                validate_cause_binding(cause, binding, agreed)?;
                if matches!(self, Self::StoppedPublished { receipt_id, .. } if *receipt_id == ReceiptId::from_bytes([0; 32]))
                {
                    return Err(ProtocolError::InvalidTerminalStatus);
                }
                Ok(())
            }
        }
    }
}

pub(crate) fn validate_cause_binding(
    cause: &StopCause,
    binding: &ExecutionBinding,
    agreed: StepCursor,
) -> Result<(), ProtocolError> {
    match cause {
        StopCause::Authenticated(occurrence) => {
            if occurrence.session_id() != binding.session_id()
                || *occurrence.coordinate() != agreed
                || !binding.is_participant(occurrence.sender())
                || !occurrence
                    .verify_signature()
                    .map_err(|_| ProtocolError::InvalidTerminalStatus)?
            {
                return Err(ProtocolError::InvalidTerminalStatus);
            }
        }
        StopCause::Shared { commitment, .. } => {
            if !commitment.is_bound_to(binding.session_id())
                || commitment.step.checked_add(1) != Some(agreed.next_step())
                || commitment.post_state != agreed.state_hash()
                || commitment.link_hash() != agreed.chain_hash()
            {
                return Err(ProtocolError::InvalidTerminalStatus);
            }
        }
    }
    Ok(())
}

fn ensure_reason(reason: &str) -> Result<(), ProtocolError> {
    ensure_payload(
        "terminal reason",
        reason.len(),
        super::MAX_TERMINAL_REASON_BYTES,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PeerId, SessionHash, StateHash};
    use arena0_crypto::Ed25519Signature;

    fn occurrence() -> AbortOccurrence {
        AbortOccurrence::new(
            SessionHash([1; 32]),
            PeerId([2; 32]),
            AbortKind::Fail,
            0,
            "stop",
            StepCursor::new(0, StateHash([3; 32]), crate::CHAIN_START),
            Ed25519Signature([0; 64]),
        )
        .expect("valid abort occurrence")
    }

    #[test]
    fn stop_causes_round_trip() {
        let causes = [
            StopCause::Authenticated(occurrence()),
            StopCause::Shared {
                kind: AbortKind::Abort,
                commitment: StepCommitment {
                    domain: crate::STEP_COMMIT_DOMAIN,
                    session_id: SessionHash([1; 32]),
                    step: 0,
                    entry_hash: [0; 32],
                    pre_state: StateHash([3; 32]),
                    post_state: StateHash([3; 32]),
                    link: crate::CHAIN_START,
                },
                reason: "stop".into(),
            },
        ];
        for cause in &causes {
            assert_eq!(
                borsh::from_slice::<StopCause>(&borsh::to_vec(cause).unwrap()).unwrap(),
                *cause
            );
        }
    }
}
