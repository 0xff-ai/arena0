//! The arena0 node: negotiation, transport routing, and durable execution
//! actors.
//!
//! One private execution actor is the only runtime owner for each
//! [`arena0_protocol::ExecId`]. It owns the execution state, a loaded Wasm
//! program, and live transport capabilities, and persists each protocol
//! transition through [`arena0_store::ExecutionStore`].

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
    PrepareOutcome, RecomputeInitialStateEffect, serve_fetch_evidence, store_activation_effects,
    unix_time_ms,
};
