//! Deterministic Wasm sandbox for fresh, bounded arena0 guest calls.
//!
//! Admission compiles immutable Wasm once. Every semantic invocation creates a
//! fresh Wasmtime store and instance, restores explicit shared and local state
//! bytes, calls exactly one capability-specific export, collects one atomic
//! result, and drops the instance. No mutable guest instance crosses this API.
//!
//! The ABI exports are `arena0_initialize`, `arena0_shared`, `arena0_local`,
//! `arena0_writer`, `arena0_query`, `arena0_view`, `arena0_outcome`, and
//! `arena0_metadata`, plus
//! `arena0_alloc`/`arena0_dealloc`; all result-bearing exports use the packed
//! `(i32, i32) -> i64` pointer/length convention. Downstream execution callers
//! must construct the capability-specific call types directly: JSON requests
//! use [`arena0_program::JsonBytes`], active calls carry a committed ensemble,
//! and projections return bounded typed values.

mod call;
mod engine;
mod error;
mod validation;

pub use call::{
    InitializeCall, LocalCall, LocalEvent, OutcomeCall, QueryCall, RandomReplay, RandomReplayError,
    SharedCall, SharedEvent, ViewCall, WriterCall,
};
pub use engine::{AdmittedProgram, WasmtimeEngine};
pub use error::SandboxError;

mod program;
pub use program::Program;

use arena0_program::{CallStatus, JsonBytes, LocalStateBytes, OutcomeBytes, SharedStateBytes};
use arena0_protocol::Effect;

/// Complete result of one fresh shared/public guest invocation.
///
/// A shared call can replace only replicated state. The host retains any
/// participant-local state outside this result. Effects, fuel, replayable
/// randomness, and logs belong to this invocation. A failed call returns an
/// error and its instance is discarded.
#[derive(Debug)]
pub struct SharedCallResult {
    /// Whether a mutating event was accepted or rejected.
    pub status: CallStatus,
    /// Replacement shared semantic state.
    pub shared: SharedStateBytes,
    /// Effects, replay evidence, resource use, and logs from this invocation.
    pub observations: CallObservations,
}

/// Complete result of one fresh local/private guest invocation.
///
/// A local call can replace only participant-local state. The host retains the
/// shared state outside this result, so a local guest cannot smuggle a shared
/// state replacement through the ABI.
#[derive(Debug)]
pub struct LocalCallResult {
    /// Whether a mutating event was accepted or rejected.
    pub status: CallStatus,
    /// Replacement participant-local semantic state.
    pub local: LocalStateBytes,
    /// Effects, replay evidence, resource use, and logs from this invocation.
    pub observations: CallObservations,
}

/// State produced by the state-only initialization call.
#[derive(Debug)]
pub struct InitializedState {
    /// Initial shared semantic state.
    pub shared: SharedStateBytes,
    /// Initial participant-local semantic state.
    pub local: LocalStateBytes,
    /// Fuel consumed by initialization.
    pub fuel_used: u64,
}

/// Two bounded projections of one terminal outcome DTO.
#[derive(Debug)]
pub struct GuestOutcomeResult {
    /// Stock-Borsh bytes used by the terminal protocol effect.
    pub borsh: OutcomeBytes,
    /// Stock-Serde JSON bytes exposed to agents.
    pub json: JsonBytes,
    /// Fuel consumed by this projection.
    pub fuel_used: u64,
}

/// Result of the read-only next-writer projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestWriterResult {
    /// Sole participant eligible to author the next public message.
    pub writer: Option<arena0_protocol::Participant>,
    /// Fuel consumed by this projection.
    pub fuel_used: u64,
}

/// Result of a read-only query, view, or outcome projection.
///
/// Projections intentionally do not expose replacement state, effects,
/// randomness, or logs. The sandbox rejects any guest attempt to produce one
/// before constructing this type.
#[derive(Debug)]
pub struct GuestProjectionResult {
    /// Agent-facing projection bytes.
    pub output: JsonBytes,
    /// Fuel consumed by this projection.
    pub fuel_used: u64,
}

/// Host observations collected outside the guest envelope for one invocation.
/// Mutable call results retain these facts together. Read-only projections
/// validate that effects, randomness, and logs are absent and expose only fuel.
#[derive(Debug)]
pub struct CallObservations {
    /// Host effects captured during the invocation.
    pub effects: Vec<Effect>,
    /// Fuel consumed by the invocation.
    pub fuel_used: u64,
    /// Replayable random draws served by the host.
    pub random_draws: Vec<Vec<u8>>,
    /// Diagnostic logs captured during the invocation.
    pub logs: Vec<(String, String)>,
}
