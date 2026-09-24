//! Durable execution-domain values.
//!
//! The actor dispatches one flat [`crate::Event`] through the program. This
//! module contains the durable coordinates and evidence needed around that
//! dispatch; it intentionally has no reducer, plan, or split transition layer.

mod abort;
mod binding;
mod certificate;
mod cursor;
mod error;
mod outcome;
mod pending;
mod signing;
mod state;
mod status;
mod timer;
mod timer_firing;
mod validation;

/// Maximum number of effects retained for one event dispatch.
pub const MAX_EFFECTS: usize = 128;
/// Maximum number of active one-shot timers in one execution.
pub const MAX_ACTIVE_TIMERS: usize = 64;
/// Maximum number of signatures retained for one pending proof.
pub const MAX_PROOF_SIGNATURES: usize = crate::MAX_PARTICIPANTS;
/// Maximum encoded bytes accepted for one execution state.
/// Maximum encoded bytes accepted for one execution aggregate. A staged
/// proposal can retain both the current and proposed shared/local memories,
/// plus up to the program's complete dispatch-effect budget.
pub const MAX_EXECUTION_STATE_BYTES: usize = 32 * 1024 * 1024;
/// Maximum opaque bytes in one effect payload.
pub const MAX_EFFECT_PAYLOAD_BYTES: usize = 64 * 1024;
/// Maximum opaque bytes in one receipt artifact.
pub const MAX_RECEIPT_BYTES: usize = 8 * 1024 * 1024;
/// Maximum opaque bytes in one timer payload.
pub const MAX_TIMER_PAYLOAD_BYTES: usize = 64 * 1024;
/// Maximum UTF-8 bytes in one terminal reason.
pub const MAX_TERMINAL_REASON_BYTES: usize = 4 * 1024;
/// Maximum opaque bytes in one successful terminal outcome.
pub const MAX_TERMINAL_OUTCOME_BYTES: usize = 64 * 1024;
/// Maximum total encoded bytes for one trace entry.
pub const MAX_TRACE_ENTRY_BYTES: usize = 256 * 1024;
/// Maximum number of entries in a complete receipt trace.
pub const MAX_RECEIPT_TRACE_ENTRIES: usize = 65_536;

pub use abort::{ABORT_OCCURRENCE_DOMAIN, ABORT_OCCURRENCE_VERSION, AbortKind, AbortOccurrence};
pub use binding::ExecutionBinding;
pub use certificate::{
    ParticipantStepSignature, Receipt, ReceiptArtifact, ReceiptBody, ReceiptId, ReceiptKind,
    StopReport,
};
pub use cursor::{ExecutionVersion, StepCursor};
pub use error::ProtocolError;
pub use outcome::TerminalOutcome;
pub use pending::{OpenCallout, PendingId, PendingIdParseError, pending_id};
pub use signing::GuestSignData;
pub use state::{ExecutionState, SharedProposal, StepCertificate};
pub use status::{ExecutionStatus, ReceiptWork, StopCause};
pub use timer::TimerId;
pub use timer_firing::TimerFiring;

pub(crate) use validation::{
    ensure_encoded, ensure_payload, validate_effects, validate_proposal, validate_receipt_body,
    validate_receipt_body_shape,
};
