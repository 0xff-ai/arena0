//! SDK for writing arena0 programs that compile to `wasm32-unknown-unknown`.
//!
//! We provide the [`Program`] trait (events in, effects out) and [`Context`]
//! for accessing shared and local state plus issuing host effects during each dispatch
//! step. The [`prelude`] re-exports everything a typical program needs.
//!
//! Guest ABI buffers are available only inside Wasm.
//!
//! ```compile_fail
//! let _ = arena0::io_alloc::io_alloc(8);
//! ```
//!
//! A local handler's [`LocalContext`] exposes shared state read-only, so a
//! generated primitive accessor there has no shared-mutation method. The sample
//! state derives every trait the state macro requires, so the mutation is the
//! only compilation error:
//!
//! ```compile_fail
//! use arena0::prelude::*;
//!
//! #[derive(Default)]
//! #[arena0::primitive]
//! struct Counter {
//!     value: u64,
//! }
//!
//! #[arena0::state(max = 64)]
//! struct Shared {
//!     #[primitive]
//!     counter: Counter,
//! }
//!
//! #[arena0::local]
//! #[derive(Default)]
//! struct Local {}
//!
//! fn local(ctx: &mut LocalContext<Shared, Local>) {
//!     // `LocalContext` has no `mutate`, so this does not compile.
//!     ctx.counter().mutate(|counter| counter.value = 1);
//! }
//! ```
//!
//! Reading that primitive from a local handler is allowed:
//!
//! ```rust
//! use arena0::prelude::*;
//!
//! #[derive(Default)]
//! #[arena0::primitive]
//! struct Counter {
//!     value: u64,
//! }
//!
//! #[arena0::state(max = 64)]
//! struct Shared {
//!     #[primitive]
//!     counter: Counter,
//! }
//!
//! #[arena0::local]
//! #[derive(Default)]
//! struct Local {}
//!
//! fn local(ctx: &mut LocalContext<Shared, Local>) {
//!     let _ = ctx
//!         .counter()
//!         .with_shared_local(|counter, _local| counter.value);
//! }
//! ```
//!
//! Programs are pure state machines. The runtime handles transport,
//! callout/input collection, and state verification.

pub mod context;
mod effects;
pub mod fault;
#[doc(hidden)]
#[cfg(target_arch = "wasm32")]
pub mod io_alloc;
pub mod primitive;
mod program;
mod schema_primitives;
#[path = "prelude.rs"]
mod sdk_prelude;
mod state;
pub mod timer;
mod transition;

extern crate self as arena0;

/// Temporary work-memory reserve established before a resident guest starts
/// serving dispatches. The runtime measures the resulting work memory and
/// freezes that capacity for the lifetime of the instance.
pub const RESIDENT_ALLOCATOR_RESERVE_BYTES: usize = arena0_program::MIN_PREPARED_WORK_MEMORY_BYTES;

pub use arena0_sdk_macros::{
    callout, callouts, data, local, message, outcome, phases, primitive, program, query, state,
};
pub use context::{
    AgreedMode, BroadcastError, CalloutContext, Context, Crypto, Ctx, EffectMode, Effects,
    LocalContext, LocalMode, Mode, PrimitiveField, PrimitiveOutput, PrimitiveOutputs,
    PrimitiveRoute, RawPrimitiveRoute, ReadMode, Signed,
};
#[doc(hidden)]
pub use effects::{
    STATE_KIND_LOCAL as __STATE_KIND_LOCAL, STATE_KIND_SHARED as __STATE_KIND_SHARED,
    host_state_len as __host_state_len, host_state_read as __host_state_read,
    host_state_write as __host_state_write,
};
#[doc(hidden)]
pub use effects::{host_fail as __host_fail, host_log as __host_log};
pub use fault::{ProgramFault, ProtocolFault};
#[doc(hidden)]
#[cfg(target_arch = "wasm32")]
pub use io_alloc::prepare_allocator as __prepare_allocator;
pub use primitive::Primitive;
pub use program::{
    ApplyDecision, MessageApply, PhasedProgram, PhaselessTransition, Program, ProgramQuery,
    ProgramTransition, ProgramView,
};
pub use sdk_prelude::prelude;
pub use state::{
    Arena0Callout, Arena0CalloutRequest, Arena0Phase, Arena0Query, Arena0TypedCalloutRequest,
    CalloutSpec, LocalDebug, LocalState, ManagedPhase, PhaseDecl, PhasedSharedState, ProgramValue,
    SharedState,
};
pub use timer::{IntoTimerEffect, TimerSchedule, decode_timer_payload, timer_payload};
pub use transition::{AbortReason, Transition};

pub use anyhow;
pub use arena0_crypto::{HashAlgorithm, SignScheme};
pub use arena0_program::{
    ABI_VERSION, AbiEnvelopeError, BorshSchemaDocument, CallStatus, CalloutRequest, CalloutSchema,
    Capability, CapabilityImport, CapabilitySet, DispatchInput, DispatchOutput, ExecutionProfile,
    ExecutionProfileHash, HOST_MODULE, InitInput, InitializedState, JsonBytes, JsonBytesError,
    JsonSchemaDocument, JsonSchemaDocumentError, LocalStateBytes, MAX_CALL_ENVELOPE_BYTES,
    MAX_LOCAL_STATE_BYTES, MAX_REJECTION_REASON_BYTES, MessageSchema, OutcomeBytes,
    OutcomeBytesError, OutcomeInput, OutcomeOutput, PROGRAM_DEFINITION_MAGIC,
    PROGRAM_DEFINITION_VERSION, PROGRAM_MAX_LEN, ParticipantCount, ParticipantCountError,
    PrimitiveRouteSchema, ProgramDefinition, ProgramDefinitionError, ProgramHash, ProgramMetadata,
    ProgramSchema, QueryInput, QueryOutput, QuerySchema, SharedStateBytes, StateBytesError,
    StateSchema, ViewInput, ViewOutput, WriterInput, WriterOutput, abi,
};
pub use arena0_protocol as types;
pub use arena0_protocol::{
    Committed, DisconnectReason, Effect, Ensemble, EnsembleError, Event, LogLevel, Open,
    Participant, PeerId, SessionHash, StateHash, TimerPayload, TimerSpec, TraceEntry, View,
    Viewport,
};
pub use blake3;
pub use borsh;
pub use schemars;
pub use serde;
pub use serde_json;

/// Parse a JSON-encoded callout answer into the concrete DTO.
///
/// Used by generated `Arena0Callout::from_raw` implementations. The host
/// forwards agent JSON unchanged; only the guest's stock Serde impl for the
/// concrete type interprets it.
#[doc(hidden)]
pub fn __parse_input_data<T: serde::de::DeserializeOwned>(data: &[u8]) -> anyhow::Result<T> {
    serde_json::from_slice(data).map_err(Into::into)
}

/// Format and bound one guest input rejection reason before it crosses the ABI.
#[doc(hidden)]
#[must_use]
pub fn __truncate_rejection_reason(error: &anyhow::Error) -> String {
    let reason = format!("{error:#}");
    let mut end = reason.len().min(MAX_REJECTION_REASON_BYTES);
    while end > 0 && !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason[..end].to_owned()
}

/// Inverse of [`__parse_input_data`]: serialize a value to JSON event bytes.
#[doc(hidden)]
pub fn __serialize_input_data<T: serde::Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).expect("input data serialization failed")
}

#[macro_export]
#[doc(hidden)]
macro_rules! __arena0_capability_vec {
    () => { ::std::vec::Vec::new() };
    (,) => { ::std::vec::Vec::new() };
    (auto) => { ::std::vec::Vec::new() };
    (auto, $($rest:tt)*) => {
        $crate::__arena0_capability_vec!($($rest)*)
    };
    (Messaging, $($rest:tt)*) => {{
        let mut capabilities = ::std::vec![$crate::Capability::Messaging];
        capabilities.extend($crate::__arena0_capability_vec!($($rest)*));
        capabilities
    }};
    (Messaging) => { ::std::vec![$crate::Capability::Messaging] };
    (Timers, $($rest:tt)*) => {{
        let mut capabilities = ::std::vec![$crate::Capability::Timers];
        capabilities.extend($crate::__arena0_capability_vec!($($rest)*));
        capabilities
    }};
    (Timers) => { ::std::vec![$crate::Capability::Timers] };
    (Sign { schemes: [$($scheme:ident),* $(,)?] }, $($rest:tt)*) => {{
        let mut capabilities = ::std::vec![
            $crate::Capability::Sign {
                schemes: ::std::vec![$($crate::SignScheme::$scheme),*],
            }
        ];
        capabilities.extend($crate::__arena0_capability_vec!($($rest)*));
        capabilities
    }};
    (Sign { schemes: [$($scheme:ident),* $(,)?] }) => {
        ::std::vec![
            $crate::Capability::Sign {
                schemes: ::std::vec![$($crate::SignScheme::$scheme),*],
            }
        ]
    };
}
