//! Durable execution-domain values.
//!
//! The actor dispatches one flat [`crate::Event`] through the program. This
//! module contains the durable coordinates and evidence needed around that
//! dispatch.

mod abort;
mod binding;
mod blob_change;
mod callout;
mod certificate;
mod cursor;
mod end;
mod error;
mod outcome;
mod signing;
mod state;
mod status;
#[cfg(test)]
mod test_fixtures;
mod timer;
mod validation;

/// Maximum number of effects retained for one event dispatch.
pub const MAX_EFFECTS: usize = 128;
/// Maximum number of messages one execution may hold in its outgoing queue.
pub const MAX_OUTGOING_MESSAGES: usize = 16;
/// Maximum unacknowledged direct messages queued for one recipient.
pub const MAX_DIRECT_QUEUE: usize = 8;
/// Maximum number of active one-shot timers in one execution.
pub const MAX_ACTIVE_TIMERS: usize = 64;
/// Maximum number of signatures retained for one step proposal.
pub const MAX_PROOF_SIGNATURES: usize = crate::MAX_PARTICIPANTS;
/// Maximum encoded bytes accepted for one execution aggregate. A staged
/// proposal can retain both the current and proposed shared/local memories,
/// plus up to the program's complete dispatch-effect budget.
pub const MAX_EXECUTION_STATE_BYTES: usize = 32 * 1024 * 1024;
/// Maximum opaque bytes in one receipt artifact.
pub const MAX_RECEIPT_BYTES: usize = 8 * 1024 * 1024;
/// Maximum total encoded bytes for one trace entry.
pub const MAX_TRACE_ENTRY_BYTES: usize = 256 * 1024;
/// Maximum number of entries in a complete receipt trace.
pub const MAX_RECEIPT_TRACE_ENTRIES: usize = 65_536;

pub use abort::{ABORT_OCCURRENCE_DOMAIN, ABORT_OCCURRENCE_VERSION, AbortKind, AbortOccurrence};
pub use binding::ExecutionBinding;
pub use blob_change::BlobChange;
pub use callout::{CalloutId, CalloutIdParseError, OpenCallout, callout_id};
pub use certificate::{
    ParticipantStepSignature, Receipt, ReceiptArtifact, ReceiptBody, ReceiptId, ReceiptKind,
    ReceiptProvenance, ReceiptSummary, StopReport,
};
pub use cursor::{ExecutionVersion, StepCursor};
pub use end::{EndMatch, EndPhase};
pub use error::ProtocolError;
pub use outcome::TerminalOutcome;
pub use signing::GuestSignData;
pub use state::{ExecutionState, SharedProposal, StepCertificate};
pub use status::{ExecutionStatus, ReceiptWork, StopCause};
pub use timer::TimerId;

pub use validation::{check_effect_budget, validate_agreed_trace};
pub(crate) use validation::{
    ensure_encoded, ensure_payload, single_lifecycle_effect, validate_effects, validate_proposal,
    validate_receipt_body, validate_receipt_body_shape, verify_full_agreement,
    verify_step_signature,
};
