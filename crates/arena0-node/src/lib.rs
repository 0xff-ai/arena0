//! The arena0 node: negotiation, transport routing, and durable execution
//! actors.
//!
//! One private execution actor is the only runtime owner for each
//! [`arena0_protocol::ExecId`]. It keeps
//! a loaded Wasm program and live transport capabilities in memory, while
//! every protocol fact is loaded from and committed through its claimed
//! [`arena0_store::ExecutionStore`].
//! The protocol reducer and store are intentionally below this crate; this
//! crate only orchestrates guest calls and external delivery.

mod context;
mod ensemble;
mod execution;
mod host;
mod machines;
mod router;

pub use context::{
    ExecCommand, ExecContext, ExecError, HostExecutionStore, SessionMessage, SpawnedExec,
};
pub use ensemble::{Ensemble, EnsembleError};
pub use host::{Host, HostError};
pub use machines::activation::{ActivatedSession, ActivatedSessionError};
pub use machines::negotiation::{
    ApplyError, ApplyOutcome, DurableOutcome, FETCH_TIMEOUT, LocalTicketWithdrawal,
    NegotiationAttempt, NegotiationBook, NegotiationDriveError, NegotiationEffects,
    NegotiationStart, NegotiationSupervision, PersistActivationEffect, PrepareEffect,
    PrepareOutcome, RecomputeInitialStateEffect, serve_fetch_evidence, unix_time_ms,
};
