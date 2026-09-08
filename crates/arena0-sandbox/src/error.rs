//! Error type for the Wasm sandbox domain.

use thiserror::Error;

/// Wasm sandbox errors: compilation, linking, capability violations, resource limits.
#[derive(Error, Debug, Clone)]
#[non_exhaustive]
pub enum SandboxError {
    /// Wasmtime's persistent compilation cache could not be configured.
    #[error("persistent compilation cache unavailable: {0}")]
    CacheUnavailable(String),
    /// Wasmtime failed to compile the module.
    #[error("module compilation failed: {0}")]
    CompilationFailed(String),
    /// Module instantiation failed (e.g., missing imports).
    #[error("instantiation failed: {0}")]
    InstantiationFailed(String),
    /// A required Wasm export is missing.
    #[error("missing export: {0}")]
    MissingExport(String),
    /// A required Wasm export has the wrong ABI type.
    #[error("invalid export signature for {name}: expected {expected}, got {actual}")]
    InvalidExportSignature {
        name: String,
        expected: &'static str,
        actual: String,
    },
    /// A module imports from outside the arena0 host ABI.
    #[error("invalid import: {module}::{name}")]
    InvalidImport { module: String, name: String },
    /// The module declares an ABI version we do not support.
    #[error("invalid ABI version: expected {expected}, got {actual}")]
    InvalidAbiVersion { expected: u32, actual: u32 },
    /// The `arena0.metadata` custom section is absent.
    #[error("missing arena0.metadata custom section")]
    MissingMetadata,
    /// The metadata custom section is present but malformed.
    #[error("invalid metadata: {0}")]
    InvalidMetadata(String),
    /// The program called a host function it did not declare a capability for.
    #[error("capability violation: {action} requires undeclared capability {capability}")]
    CapabilityViolation { action: String, capability: String },
    /// The program wrote to the session memory region during a read-only phase.
    #[error("read-only violation: program wrote to state region during {operation}")]
    ReadOnlyViolation { operation: String },
    /// A Wasm program exceeds the host program-size bound.
    #[error("program size {size} exceeds the limit of {max} bytes")]
    ProgramTooLarge { size: u64, max: u64 },
    /// A memory allocation would exceed the configured limit.
    #[error("memory limit exceeded: {0} bytes requested")]
    MemoryLimitExceeded(u64),
    /// A guest projection returned more bytes than the host allows.
    #[error("output limit exceeded: {size} bytes (max {max})")]
    OutputLimitExceeded { size: u64, max: u64 },
    /// Input or output decoding exceeded a profile-owned byte budget.
    #[error("input limit exceeded: {0}")]
    InputLimitExceeded(String),
    /// A guest exceeded the per-call host import budget.
    #[error("host-call limit exceeded: {count} calls (max {max})")]
    HostCallLimitExceeded { count: u64, max: u64 },
    /// A guest exceeded the per-call log budget.
    #[error("log limit exceeded: {actual} bytes or entries (max {max})")]
    LogLimitExceeded { actual: u64, max: u64 },
    /// A guest exceeded the per-call encoded effect budget.
    #[error("effect bytes limit exceeded: {actual} bytes (max {max})")]
    EffectBytesLimitExceeded { actual: u64, max: u64 },
    /// The program produced more lifecycle intents than allowed in one step.
    #[error("intent overflow: produced {count} intents (max {max})")]
    IntentOverflow { count: u32, max: u32 },
    /// General dispatch-step failure.
    #[error("dispatch failed: {0}")]
    DispatchFailed(String),
    /// Borsh serialization of an event or action failed.
    #[error("serialization failed: {0}")]
    SerializationFailed(String),
    /// Borsh deserialization of a return value failed.
    #[error("deserialization failed: {0}")]
    DeserializationFailed(String),
}

impl SandboxError {
    /// Wrap a compilation error message.
    pub(crate) fn compilation_failed(err: impl Into<String>) -> Self {
        Self::CompilationFailed(err.into())
    }

    /// Wrap an instantiation error message.
    pub(crate) fn instantiation_failed(err: impl Into<String>) -> Self {
        Self::InstantiationFailed(err.into())
    }

    /// Wrap a dispatch error message.
    pub(crate) fn dispatch_failed(err: impl Into<String>) -> Self {
        Self::DispatchFailed(err.into())
    }

    /// Wrap an input or call-envelope size failure.
    pub(crate) fn input_limit(err: impl Into<String>) -> Self {
        Self::InputLimitExceeded(err.into())
    }
}
