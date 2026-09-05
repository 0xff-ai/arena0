//! Pure execution aggregate and reducers.
//!
//! [`ExecutionState`] is the single durable execution aggregate. The pure
//! [`transition`] function turns an explicit [`ExecutionInput`] into a
//! [`CommitPlan`]. Adapters persist and execute only the material in that
//! plan.

mod abort;
mod binding;
mod certificate;
mod cursor;
mod delta;
mod effect;
mod error;
mod input;
mod occurrence;
mod outcome;
mod pending;
mod plan;
mod reducer;
mod signing;
mod state;
mod status;
mod timer;
mod timer_firing;
mod validation;

pub(crate) const PLAN_DOMAIN: &[u8] = b"arena0/commit-plan/v1";
pub(crate) const OUTBOX_DOMAIN: &[u8] = b"arena0/outbox-occurrence/v1";
pub(crate) const TIMER_DOMAIN: &[u8] = b"arena0/timer/v1";

/// Maximum number of public effects in one trace entry accepted by the kernel.
pub const MAX_SHARED_EFFECTS: usize = 128;
/// Maximum number of effects in one private record accepted by the kernel.
pub const MAX_PRIVATE_EFFECTS: usize = 128;
/// Maximum number of durable outbox occurrences in one plan.
pub const MAX_OUTBOX_OCCURRENCES: usize = 128;
/// Maximum number of timer mutations in one plan.
pub const MAX_TIMER_MUTATIONS: usize = 64;
/// Maximum number of active one-shot timers in one execution.
pub const MAX_ACTIVE_TIMERS: usize = 64;
/// Maximum number of signatures retained for one pending proof.
pub const MAX_PROOF_SIGNATURES: usize = crate::MAX_PARTICIPANTS;
/// Maximum encoded bytes accepted for one execution input.
pub const MAX_EXECUTION_INPUT_BYTES: usize = 16 * 1024 * 1024;
/// Maximum encoded bytes accepted for one execution state.
pub const MAX_EXECUTION_STATE_BYTES: usize = 16 * 1024 * 1024;
/// Maximum encoded bytes emitted for one commit plan.
pub const MAX_COMMIT_PLAN_BYTES: usize = 32 * 1024 * 1024;
/// Maximum opaque bytes in one durable effect payload.
pub const MAX_EFFECT_PAYLOAD_BYTES: usize = 64 * 1024;
/// Maximum opaque bytes in one receipt publication intent.
pub const MAX_RECEIPT_BYTES: usize = 8 * 1024 * 1024;
/// Maximum opaque bytes in one timer payload.
pub const MAX_TIMER_PAYLOAD_BYTES: usize = 64 * 1024;
/// Maximum UTF-8 bytes in one terminal reason.
pub const MAX_TERMINAL_REASON_BYTES: usize = 4 * 1024;
/// Maximum opaque bytes in one successful terminal outcome.
pub const MAX_TERMINAL_OUTCOME_BYTES: usize = 64 * 1024;
/// Maximum total encoded bytes for a shared trace entry.
pub const MAX_TRACE_ENTRY_BYTES: usize = 256 * 1024;
/// Maximum total encoded bytes for a private record.
pub const MAX_PRIVATE_RECORD_BYTES: usize = 256 * 1024;
/// Maximum number of entries in a complete receipt trace.
pub const MAX_RECEIPT_TRACE_ENTRIES: usize = 65_536;

pub use abort::{ABORT_OCCURRENCE_DOMAIN, ABORT_OCCURRENCE_VERSION, AbortKind, AbortOccurrence};
pub use binding::ExecutionBinding;
pub use certificate::{
    ParticipantStepSignature, ParticipantTerminalSignature, Receipt, ReceiptArtifact, ReceiptBody,
    ReceiptId, ReceiptKind, StopReport,
};
pub use cursor::{ExecutionVersion, PrivateCursor, PublicCursor};
pub use delta::{PrivateCause, PrivateContext, PrivateDelta, SharedDelta, pending_id};
pub use effect::{BroadcastFrame, DurableEffect, FrameId, OutboxId, OutboxIntent};
pub use error::ProtocolError;
pub use input::ExecutionInput;
pub use occurrence::{
    OCCURRENCE_DIGEST_DOMAIN, OCCURRENCE_KEY_DOMAIN, OccurrenceConflict, OccurrenceDigest,
    OccurrenceEvidence, OccurrenceKey, OccurrenceKind,
};
pub use outcome::TerminalOutcome;
pub use pending::{PendingId, PendingIdParseError};
pub use plan::{CommitPlan, PlanId, TransitionOutcome};
pub use signing::GuestSignData;
pub use state::{
    ExecutionState, PendingCoordinate, PrivateCommit, SharedCommit, SharedProposal,
    StepCertificate, TerminalCertificate, TerminalPublication,
};
pub use status::{ExecutionStatus, PublishedProof, ReceiptWork, StopCause, TerminalProof};
pub use timer::{TimerId, TimerMutation};
pub use timer_firing::TimerFiring;

pub(crate) use certificate::{make_step_certificate, make_terminal_certificate};
pub(crate) use delta::pending_effect_index;
pub(crate) use plan::PlanDraft;
pub(crate) use plan::build_plan;
pub(crate) use timer::ActiveTimer;
pub(crate) use validation::{
    apply_timer_mutations, ensure_encoded, ensure_payload, illegal, require_active,
    terminal_effect_count, validate_pending_record, validate_private_record, validate_proposal,
    validate_receipt_body, validate_receipt_body_shape, validate_shared_entry,
    validate_terminal_progress, validate_trace_entry,
};

/// Apply one pure execution input to a validated durable aggregate.
pub fn transition(
    state: &ExecutionState,
    input: ExecutionInput,
) -> Result<TransitionOutcome, ProtocolError> {
    reducer::transition(state, input)
}

#[cfg(test)]
mod tests;
