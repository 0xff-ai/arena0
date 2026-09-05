//! SDK for writing arena0 programs that compile to `wasm32-unknown-unknown`.
//!
//! We provide the [`Program`] trait (events in, effects out) and [`Context`]
//! for accessing shared and local state plus issuing host effects during each dispatch
//! step. The [`prelude`] re-exports everything a typical program needs.
//!
//! Guest ABI buffers are available only inside Wasm. Native tests use
//! [`testing`] and cannot access the guest's single-threaded allocator.
//!
//! ```compile_fail
//! let _ = arena0::io_alloc::io_alloc(8);
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
#[cfg(not(target_arch = "wasm32"))]
pub mod testing;
pub mod timer;
mod transition;

extern crate self as arena0;

pub use arena0_sdk_macros::{
    callout, callouts, data, local, message, outcome, pending, phases, primitive, program, query,
    state, test,
};
pub use context::{
    ArenaFuture, CalloutBuilder, Context, Crypto, Effects, LocalPrimitiveField, PrimitiveOutput,
    PrimitiveOutputs, PrimitiveRoute, RawPrimitiveRoute, SharedContext, SharedPrimitiveField,
    SignBuilder,
};
#[doc(hidden)]
pub use effects::{
    host_fail as __host_fail, host_log as __host_log, host_retry_input as __host_retry_input,
};
pub use fault::{InputFault, ProgramFault, ProtocolFault, Retryable};
pub use primitive::Primitive;
pub use program::{
    ApplyDecision, MessageApply, PhasedProgram, PhaselessTransition, Program, ProgramQuery,
    ProgramTransition, ProgramView,
};
pub use sdk_prelude::prelude;
pub use state::{
    Arena0Callout, Arena0CalloutRequest, Arena0Pending, Arena0Phase, Arena0Query,
    Arena0TypedCalloutRequest, CalloutSpec, LocalDebug, LocalState, ManagedPhase, PendingDecl,
    PhaseDecl, PhasedSharedState, ProgramValue, SharedState,
};
pub use timer::{IntoTimerEffect, TimerSchedule, decode_timer_payload, timer_payload};
pub use transition::{AbortReason, Transition};

pub use anyhow;
pub use arena0_crypto::{HashAlgorithm, SignScheme};
pub use arena0_program::{
    ABI_VERSION, AbiEnvelopeError, BorshSchemaDocument, CallStatus, CalloutSchema, Capability,
    CapabilityImport, CapabilitySet, ExecutionProfile, ExecutionProfileHash, HOST_MODULE,
    InitInput, InitializedState, JsonBytes, JsonBytesError, JsonSchemaDocument,
    JsonSchemaDocumentError, LocalInput, LocalOutput, LocalStateBytes, MAX_CALL_ENVELOPE_BYTES,
    MessageSchema, OutcomeBytes, OutcomeBytesError, OutcomeInput, OutcomeOutput,
    PROGRAM_DEFINITION_MAGIC, PROGRAM_DEFINITION_VERSION, PROGRAM_MAX_LEN, ParticipantCount,
    ParticipantCountError, PrimitiveRouteSchema, ProgramDefinition, ProgramDefinitionError,
    ProgramHash, ProgramMetadata, ProgramSchema, QueryInput, QueryOutput, QuerySchema, SharedInput,
    SharedOutput, SharedStateBytes, StateBytesError, StateSchema, ViewInput, ViewOutput,
    WriterInput, WriterOutput, abi,
};
pub use arena0_protocol as types;
pub use arena0_protocol::{
    Committed, DisconnectReason, DivergenceDiagnostic, DivergenceKind, Effect, Ensemble,
    EnsembleError, Event, LogLevel, Open, Participant, PeerId, PendingKind, PendingRecord,
    SessionHash, StateHash, TimerPayload, TimerSpec, TraceEntry, View, Viewport,
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
#[must_use]
pub fn __parse_input_data<T: serde::de::DeserializeOwned>(data: &[u8]) -> T {
    serde_json::from_slice(data).expect("input data deserialization failed")
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
    (Input, $($rest:tt)*) => {{
        let mut capabilities = ::std::vec![$crate::Capability::Input];
        capabilities.extend($crate::__arena0_capability_vec!($($rest)*));
        capabilities
    }};
    (Input) => { ::std::vec![$crate::Capability::Input] };
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
