//! The single durable owner of execution progress and terminal evidence.
//!
//! [`ExecLifecycle`](crate::ExecLifecycle) is derived from [`ExecutionStatus`]
//! for public projections. The persisted aggregate stores this enum only; it
//! does not maintain independent lifecycle, pending, proof, or terminal-cause
//! fields that could drift into an invalid cross-product.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::TraceEntry;
use crate::bounded::read_string as read_bounded_string;
use crate::exec::ExecLifecycle;
use crate::trace::StepCommitment;

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
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum StopCause {
    /// A signed local or peer abort/failure occurrence.
    Authenticated(AbortOccurrence),
    /// A certified guest-authored terminal effect.
    Shared {
        kind: AbortKind,
        commitment: StepCommitment,
        reason: String,
    },
}

impl BorshSerialize for StopCause {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> std::io::Result<()> {
        match self {
            Self::Authenticated(occurrence) => {
                BorshSerialize::serialize(&0u8, writer)?;
                BorshSerialize::serialize(occurrence, writer)
            }
            Self::Shared {
                kind,
                commitment,
                reason,
            } => {
                if reason.len() > MAX_TERMINAL_REASON_BYTES {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "terminal reason exceeds bound",
                    ));
                }
                BorshSerialize::serialize(&1u8, writer)?;
                BorshSerialize::serialize(kind, writer)?;
                BorshSerialize::serialize(commitment, writer)?;
                BorshSerialize::serialize(reason, writer)
            }
        }
    }
}

impl BorshDeserialize for StopCause {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> std::io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            0 => Ok(Self::Authenticated(AbortOccurrence::deserialize_reader(
                reader,
            )?)),
            1 => Ok(Self::Shared {
                kind: AbortKind::deserialize_reader(reader)?,
                commitment: StepCommitment::deserialize_reader(reader)?,
                reason: read_bounded_string(reader, MAX_TERMINAL_REASON_BYTES, "terminal reason")?,
            }),
            tag => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown stop cause tag {tag}"),
            )),
        }
    }
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
                kind,
                commitment,
                reason,
            } => {
                if (*kind != AbortKind::Abort && *kind != AbortKind::Fail)
                    || commitment.domain != crate::STEP_COMMIT_DOMAIN
                {
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
        let Some(reason) = entry.abort_reason() else {
            return Ok(None);
        };
        ensure_reason(reason)?;
        let kind = match entry.terminal.as_ref() {
            Some(crate::Effect::SessionAbort { .. }) => AbortKind::Abort,
            Some(crate::Effect::Fail { .. }) => AbortKind::Fail,
            _ => return Ok(None),
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
                || !binding
                    .participant_keys()?
                    .into_iter()
                    .any(|(participant, _)| participant == occurrence.sender())
                || !occurrence
                    .verify_signature()
                    .map_err(|_| ProtocolError::InvalidTerminalStatus)?
            {
                return Err(ProtocolError::InvalidTerminalStatus);
            }
        }
        StopCause::Shared { commitment, .. } => {
            if commitment.domain != crate::STEP_COMMIT_DOMAIN
                || commitment.session_id != binding.session_id()
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
