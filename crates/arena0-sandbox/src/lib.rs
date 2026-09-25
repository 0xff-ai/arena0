//! Deterministic Wasm sandbox for bounded arena0 guest calls.
//!
//! Loading a program compiles its immutable Wasm once per engine. An active
//! execution can then own one non-cloneable resident Wasm instance whose state
//! memories survive dispatches while work memory and mutable globals are reset
//! to their baseline.
//!
//! The ABI exports are `arena0_prepare`, `arena0_initialize`, `arena0_dispatch`,
//! `arena0_writer`, `arena0_query`, `arena0_view`, `arena0_outcome`, and
//! `arena0_metadata`, plus
//! `arena0_alloc`/`arena0_dealloc`; all result-bearing exports use the packed
//! `(i32, i32) -> i64` pointer/length convention. Downstream execution callers
//! must construct the capability-specific call types directly: JSON requests
//! use [`arena0_program::JsonBytes`], active calls carry a committed ensemble,
//! and projections return bounded typed values.

/// The sandbox effect-count limit is a resource limit; it must never exceed
/// the protocol maximum, which the emission check also enforces.
const _: () = assert!(
    arena0_program::MAX_EFFECTS_PER_DISPATCH as usize <= arena0_protocol::execution::MAX_EFFECTS,
    "the sandbox effect count must not exceed the protocol maximum"
);

mod call;
mod engine;
mod error;
mod finalize;
mod signing;
mod validation;

pub use call::{DispatchCall, InitializeCall, OutcomeCall, QueryCall, ViewCall, WriterCall};
pub use engine::{LoadedProgram, ProgramInstance, WasmtimeEngine};
pub use error::SandboxError;
pub use signing::GuestSigner;

mod program;
pub use program::Program;

#[cfg(test)]
mod test_support;

use arena0_program::{
    CallStatus, CalloutRequest, JsonBytes, LocalStateBytes, OutcomeBytes, SharedStateBytes,
};
use arena0_protocol::Effect;

/// Complete result of one resident dispatch.
///
/// An accepted dispatch carries its state images. A rejected dispatch
/// carries only the reason and no images, so rejection clones nothing.
#[derive(Debug)]
pub struct DispatchCallResult {
    /// Whether the event was accepted or rejected.
    pub status: CallStatus,
    /// Bounded guest reason when an input dispatch was rejected.
    pub reason: Option<String>,
    /// The single open callout derived from an accepted post-state, if any.
    pub callout: Option<CalloutRequest>,
    /// Shared payload after an accepted dispatch, or `None` on rejection.
    pub shared: Option<SharedStateBytes>,
    /// Local payload after an accepted dispatch, or `None` on rejection.
    pub local: Option<LocalStateBytes>,
    /// Host-owned effects and execution observations.
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
    /// Sole participant eligible to author the next program message.
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

impl CallObservations {
    /// Whether the invocation emitted no effects, random draws, or logs.
    ///
    /// Fuel is deliberately excluded: it measures execution cost rather than
    /// an observable effect of the guest call.
    #[must_use]
    // ponytail: keep this engine-only invariant crate-visible.
    pub(crate) fn is_empty_except_fuel(&self) -> bool {
        self.effects.is_empty() && self.random_draws.is_empty() && self.logs.is_empty()
    }
}
