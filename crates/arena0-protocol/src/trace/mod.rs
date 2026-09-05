//! Trace entries and entry attestations for cryptographic proof chains.
//!
//! The trace has two sections. The public consensus projection is byte-identical
//! on every node: one [`TraceEntry`] per canonical position (boundaries and
//! broadcast messages), each hash-chained, commitment-linked, and co-signed
//! N-of-N. Per-node fuel telemetry sits outside that projection. The private
//! section is per node: [`PrivateRecord`]s of local handler runs (callout
//! answers, entropy draws, decision code), never rolled into the shared hash,
//! hash-committed into the public section per message via
//! [`WitnessCommitment`].

mod commitment;
mod divergence;
mod entry;
mod header;
mod private;

pub use commitment::{
    AggregateAttestation, AttestationError, CHAIN_START, STEP_COMMIT_DOMAIN, SessionTerminal,
    SignerSet, StepCommitment, StepSig, TERMINAL_DOMAIN, TerminalCommitment,
};
pub use divergence::{DivergenceDiagnostic, DivergenceKind, JsonDiffExt};
pub use entry::{PendingKind, PendingOperation, PendingRecord, TraceEntry};
pub use header::{ReceiptTermination, SessionHeader};
pub use private::{PrivateRecord, WitnessCommitment};

/// Trace schema version for state-machine step records.
pub const TRACE_FORMAT_VERSION: u32 = 1;
