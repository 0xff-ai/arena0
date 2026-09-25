//! Trace entries and attestations for cryptographic proof chains.
//!
//! A portable trace contains only the normalized, agreed event entries. Local
//! event and effect history belongs to the owning Host store and is not a
//! second protocol trace type.

mod commitment;
mod entry;
mod header;

pub use commitment::{
    AggregateAttestation, AttestationError, CHAIN_START, STEP_COMMIT_DOMAIN, SignerSet,
    StepCommitment, StepSig,
};
pub use entry::{StepEvent, StepTerminal, TraceEntry};
pub use header::{ReceiptTermination, SessionHeader};

/// Trace schema version for state-machine step records.
pub const TRACE_FORMAT_VERSION: u32 = 3;
