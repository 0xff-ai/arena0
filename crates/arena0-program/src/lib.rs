//! Program-domain contracts for arena0.
//!
//! This crate owns the guest ABI, content-addressed Wasm program metadata,
//! agent-facing JSON Schemas, bounded semantic state bytes, and the execution
//! profile that defines the deterministic environment in which a program may
//! run. It deliberately has no protocol, runtime, transport, or storage
//! dependency: those layers consume these contracts directly.

pub mod abi;
mod id;
pub mod profile;
pub mod program;
pub mod schema;
pub mod state;

pub use abi::{
    ABI_VERSION, AbiEnvelopeError, CallStatus, HOST_MODULE, InitInput, InitializedState, JsonBytes,
    JsonBytesError, LocalInput, LocalOutput, MAX_CALL_PAYLOAD_BYTES, MAX_SESSION_CONTEXT_BYTES,
    OutcomeBytes, OutcomeBytesError, OutcomeInput, OutcomeOutput, QueryInput, QueryOutput,
    SharedInput, SharedOutput, ViewInput, ViewOutput, WriterInput, WriterOutput,
};
pub use id::IdParseError;
pub use profile::{
    CapabilityImport, DISPATCH_FUEL, EXECUTION_ENGINE_ID, EXECUTION_PROFILE_VERSION,
    EXECUTION_SEMANTICS_VERSION, ExecutionProfile, ExecutionProfileHash, FuelConfiguration,
    ImportSemantics, Limits, MAX_CALL_ENVELOPE_BYTES, MAX_EFFECT_BYTES, MAX_EFFECTS_PER_DISPATCH,
    MAX_HOST_BYTES, MAX_HOST_CALLS, MAX_INPUT_BYTES, MAX_LOG_BYTES, MAX_LOG_ENTRIES,
    MAX_METADATA_BYTES, MAX_OUTPUT_BYTES, MAX_RANDOM_DRAW_BYTES, MAX_RANDOM_DRAWS,
    MAX_WASM_INSTANCES, MAX_WASM_MEMORIES, MAX_WASM_MEMORY_BYTES, MAX_WASM_STACK_BYTES,
    MAX_WASM_TABLE_ELEMENTS, MAX_WASM_TABLES, RandomnessConfiguration, RandomnessSource,
    SchemaFormat, SerializationConfiguration, SerializationFormat, WasmFeatures,
};
pub use program::Hash as ProgramHash;
pub use program::{
    Capability, CapabilitySet, PROGRAM_DEFINITION_MAGIC, PROGRAM_DEFINITION_VERSION,
    PROGRAM_MAX_LEN, ParticipantCount, ParticipantCountError, ProgramDefinition,
    ProgramDefinitionError, ProgramMetadata,
};
pub use schema::{
    BorshDiagnosticError, BorshSchemaDocument, BorshSchemaDocumentError, CalloutSchema,
    JsonSchemaDocument, JsonSchemaDocumentError, MessageSchema, PrimitiveRouteSchema,
    ProgramSchema, QuerySchema, StateSchema,
};
pub use state::{
    LocalStateBytes, MAX_LOCAL_STATE_BYTES, MAX_SHARED_STATE_BYTES, SharedStateBytes,
    StateBytesError,
};
