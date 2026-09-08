//! Deterministic execution profile and its content hash.
//!
//! A Wasm hash identifies code, not the environment in which that code ran.
//! [`ExecutionProfile`] records the ABI, enabled Wasm semantics, host import
//! semantics, resource limits, fuel policy, replayable randomness policy, and
//! serialization formats that together define a reproducible execution. Its
//! Borsh representation is the versioned canonical representation used for
//! [`ExecutionProfileHash`].

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::{ABI_VERSION, Capability};

/// Version of the canonical execution-profile representation.
pub const EXECUTION_PROFILE_VERSION: u32 = 1;

/// Maximum deterministic Wasm stack size.
pub const MAX_WASM_STACK_BYTES: usize = 512 * 1024;
/// Maximum deterministic linear memory per instance.
pub const MAX_WASM_MEMORY_BYTES: usize = 64 * 1024 * 1024;
/// Maximum metadata returned by the bounded metadata probe.
pub const MAX_METADATA_BYTES: u32 = 4 * 1024 * 1024;
/// Maximum bytes returned by one guest projection.
pub const MAX_OUTPUT_BYTES: u32 = 4 * 1024 * 1024;
/// Maximum bytes in one event, parameter, query, or projection payload.
pub const MAX_INPUT_BYTES: u32 = 4 * 1024 * 1024;
/// Maximum cumulative bytes copied by host imports in one call.
pub const MAX_HOST_BYTES: u32 = 16 * 1024 * 1024;
/// Maximum effects emitted by one guest dispatch.
pub const MAX_EFFECTS_PER_DISPATCH: u32 = 100;
/// Maximum encoded bytes occupied by all effects from one call.
pub const MAX_EFFECT_BYTES: u32 = 4 * 1024 * 1024;
/// Maximum bytes retained by diagnostic logs from one call.
pub const MAX_LOG_BYTES: u32 = 1024 * 1024;
/// Maximum diagnostic log entries from one call.
pub const MAX_LOG_ENTRIES: u32 = 1_000;
/// Maximum host calls made by one guest invocation.
pub const MAX_HOST_CALLS: u32 = 10_000;
/// Maximum random draws made by one guest invocation.
pub const MAX_RANDOM_DRAWS: u32 = 1_000;
/// Maximum encoded bytes in one complete guest-call input or result envelope.
pub const MAX_CALL_ENVELOPE_BYTES: u32 = 16 * 1024 * 1024;
/// Maximum Wasm table elements available to one fresh instance.
pub const MAX_WASM_TABLE_ELEMENTS: u32 = 100_000;
/// Maximum Wasm instances created by one store.
pub const MAX_WASM_INSTANCES: u32 = 1;
/// Maximum Wasm tables available to one fresh instance.
pub const MAX_WASM_TABLES: u32 = 1;
/// Maximum Wasm memories available to one fresh instance.
pub const MAX_WASM_MEMORIES: u32 = 1;
/// Revision of the deterministic sandbox semantics.
pub const EXECUTION_SEMANTICS_VERSION: u32 = 1;
/// Compiler/engine identity bound into the execution profile.
pub const EXECUTION_ENGINE_ID: &str = "wasmtime-46.0.3-cranelift";
/// Fuel made available to one guest call.
pub const DISPATCH_FUEL: u64 = 1_000_000_000;

/// Maximum bytes one replayable random draw may contain under the default
/// profile. The sandbox may choose a stricter operational bound.
pub const MAX_RANDOM_DRAW_BYTES: u64 = 4 * 1024 * 1024;

/// Wasm feature switches that affect deterministic execution semantics.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize,
)]
pub struct WasmFeatures {
    /// Canonicalize NaN values in floating-point operations.
    pub canonicalize_nan: bool,
    /// Enable SIMD instructions.
    pub simd: bool,
    /// Enable relaxed SIMD instructions.
    pub relaxed_simd: bool,
    /// Enable multiple linear memories.
    pub multi_memory: bool,
    /// Enable 64-bit memory addressing.
    pub memory64: bool,
    /// Enable tail-call instructions.
    pub tail_call: bool,
}

impl WasmFeatures {
    /// The feature set used by the current deterministic sandbox.
    #[must_use]
    pub const fn deterministic() -> Self {
        Self {
            canonicalize_nan: true,
            simd: false,
            relaxed_simd: false,
            multi_memory: false,
            memory64: false,
            tail_call: false,
        }
    }
}

/// One capability-to-import binding in the host ABI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct CapabilityImport {
    /// Capability declaration that authorizes the imports.
    pub capability: Capability,
    /// Exact import names linked for the capability.
    pub imports: Vec<String>,
}

/// Host import names and their capability-gating semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct ImportSemantics {
    /// Wasm module name containing host imports.
    pub host_module: String,
    /// Imports linked for every admitted program.
    pub always_available: Vec<String>,
    /// Capability-gated import bindings.
    pub capability_imports: Vec<CapabilityImport>,
}

impl ImportSemantics {
    /// The import semantics defined by the current ABI.
    #[must_use]
    pub fn current() -> Self {
        let capability_imports = [
            Capability::Messaging,
            Capability::Input,
            Capability::Timers,
            Capability::Sign {
                schemes: vec![arena0_crypto::SignScheme::Ed25519],
            },
        ]
        .into_iter()
        .map(|capability| CapabilityImport {
            imports: capability
                .imports()
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            capability,
        })
        .collect();

        Self {
            host_module: crate::HOST_MODULE.to_owned(),
            always_available: crate::abi::always_available_imports()
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            capability_imports,
        }
    }
}

/// Host resource limits that affect whether execution can complete.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize,
)]
pub struct Limits {
    /// Maximum Wasm stack size in bytes.
    pub max_stack_bytes: u64,
    /// Maximum linear memory per instance in bytes.
    pub max_memory_bytes: u64,
    /// Maximum metadata returned by the metadata probe.
    pub max_metadata_bytes: u64,
    /// Maximum bytes returned by one guest projection.
    pub max_output_bytes: u64,
    /// Maximum encoded bytes in one complete guest-call envelope.
    pub max_call_envelope_bytes: u64,
    /// Maximum bytes supplied as one event, parameter, query, or projection.
    pub max_input_bytes: u64,
    /// Maximum cumulative bytes copied by host imports in one call.
    pub max_host_bytes: u64,
    /// Maximum effects emitted by one dispatch.
    pub max_effects_per_dispatch: u64,
    /// Maximum encoded effect bytes from one call.
    pub max_effect_bytes: u64,
    /// Maximum diagnostic log bytes from one call.
    pub max_log_bytes: u64,
    /// Maximum diagnostic log entries from one call.
    pub max_log_entries: u64,
    /// Maximum host imports invoked by one call.
    pub max_host_calls: u64,
    /// Maximum random draws made by one call.
    pub max_random_draws: u64,
    /// Maximum replicated state bytes.
    pub max_shared_state_bytes: u64,
    /// Maximum participant-local state bytes.
    pub max_local_state_bytes: u64,
    /// Maximum bytes in one replayable random draw.
    pub max_random_draw_bytes: u64,
    /// Maximum table elements in one fresh Wasm instance.
    pub max_table_elements: u64,
    /// Maximum nested Wasm instances in one store.
    pub max_instances: u64,
    /// Maximum Wasm tables in one store.
    pub max_tables: u64,
    /// Maximum Wasm memories in one store.
    pub max_memories: u64,
}

impl Limits {
    /// The limits used by the current deterministic sandbox.
    #[must_use]
    pub const fn current() -> Self {
        Self {
            max_stack_bytes: MAX_WASM_STACK_BYTES as u64,
            max_memory_bytes: MAX_WASM_MEMORY_BYTES as u64,
            max_metadata_bytes: MAX_METADATA_BYTES as u64,
            max_output_bytes: MAX_OUTPUT_BYTES as u64,
            max_call_envelope_bytes: MAX_CALL_ENVELOPE_BYTES as u64,
            max_input_bytes: MAX_INPUT_BYTES as u64,
            max_host_bytes: MAX_HOST_BYTES as u64,
            max_effects_per_dispatch: MAX_EFFECTS_PER_DISPATCH as u64,
            max_effect_bytes: MAX_EFFECT_BYTES as u64,
            max_log_bytes: MAX_LOG_BYTES as u64,
            max_log_entries: MAX_LOG_ENTRIES as u64,
            max_host_calls: MAX_HOST_CALLS as u64,
            max_random_draws: MAX_RANDOM_DRAWS as u64,
            max_shared_state_bytes: crate::MAX_SHARED_STATE_BYTES as u64,
            max_local_state_bytes: crate::MAX_LOCAL_STATE_BYTES as u64,
            max_random_draw_bytes: MAX_RANDOM_DRAW_BYTES,
            max_table_elements: MAX_WASM_TABLE_ELEMENTS as u64,
            max_instances: MAX_WASM_INSTANCES as u64,
            max_tables: MAX_WASM_TABLES as u64,
            max_memories: MAX_WASM_MEMORIES as u64,
        }
    }
}

/// Fuel metering configuration.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize,
)]
pub struct FuelConfiguration {
    /// Whether each guest call consumes deterministic fuel.
    pub enabled: bool,
    /// Fuel budget reset before each guest call.
    pub per_call: u64,
}

impl FuelConfiguration {
    /// The fuel policy used by the current deterministic sandbox.
    #[must_use]
    pub const fn current() -> Self {
        Self {
            enabled: true,
            per_call: DISPATCH_FUEL,
        }
    }
}

/// Source semantics for random bytes requested by a guest.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize,
)]
pub enum RandomnessSource {
    /// Live draws are recorded and replay feeds the recorded bytes in order.
    RecordedReplayable,
}

/// Replayable randomness configuration.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize,
)]
pub struct RandomnessConfiguration {
    /// Source and replay semantics for host-provided random bytes.
    pub source: RandomnessSource,
    /// Whether each draw is retained as replay evidence.
    pub record_draws: bool,
    /// Maximum bytes accepted for one draw.
    pub max_draw_bytes: u64,
}

impl RandomnessConfiguration {
    /// The randomness policy used by the current deterministic sandbox.
    #[must_use]
    pub const fn current() -> Self {
        Self {
            source: RandomnessSource::RecordedReplayable,
            record_draws: true,
            max_draw_bytes: MAX_RANDOM_DRAW_BYTES,
        }
    }
}

/// Binary and JSON formats used at deterministic program boundaries.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize,
)]
pub enum SerializationFormat {
    /// Borsh version 1 canonical encoding.
    BorshV1,
    /// JSON encoded with the concrete DTO's stock Serde implementation.
    JsonSerde,
}

/// JSON Schema draft accepted for introspection metadata.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize,
)]
pub enum SchemaFormat {
    /// JSON Schema Draft 2020-12.
    Draft202012,
}

/// Serialization rules that affect program bytes and replay.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize,
)]
pub struct SerializationConfiguration {
    /// Program-definition envelope body encoding.
    pub definition: SerializationFormat,
    /// Shared and local semantic state encoding.
    pub state: SerializationFormat,
    /// Agent-facing values encoding.
    pub agent_values: SerializationFormat,
    /// Introspection schema format.
    pub schema: SchemaFormat,
}

impl SerializationConfiguration {
    /// The serialization policy used by the current program contract.
    #[must_use]
    pub const fn current() -> Self {
        Self {
            definition: SerializationFormat::BorshV1,
            state: SerializationFormat::BorshV1,
            agent_values: SerializationFormat::JsonSerde,
            schema: SchemaFormat::Draft202012,
        }
    }
}

/// Complete deterministic execution environment contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct ExecutionProfile {
    /// Canonical profile representation version.
    pub version: u32,
    /// Guest ABI version required by this profile.
    pub abi_version: u32,
    /// Revision of the host/guest execution semantics.
    pub semantics_version: u32,
    /// Wasmtime and Cranelift implementation identity.
    pub engine_id: String,
    /// Deterministic Wasm feature switches.
    pub wasm_features: WasmFeatures,
    /// Host import and capability semantics.
    pub import_semantics: ImportSemantics,
    /// Resource limits.
    pub limits: Limits,
    /// Fuel metering policy.
    pub fuel: FuelConfiguration,
    /// Replayable randomness policy.
    pub randomness: RandomnessConfiguration,
    /// Boundary serialization policy.
    pub serialization: SerializationConfiguration,
}

impl ExecutionProfile {
    /// Build the profile for the current ABI and deterministic sandbox.
    #[must_use]
    pub fn current() -> Self {
        Self {
            version: EXECUTION_PROFILE_VERSION,
            abi_version: ABI_VERSION,
            semantics_version: EXECUTION_SEMANTICS_VERSION,
            engine_id: EXECUTION_ENGINE_ID.to_owned(),
            wasm_features: WasmFeatures::deterministic(),
            import_semantics: ImportSemantics::current(),
            limits: Limits::current(),
            fuel: FuelConfiguration::current(),
            randomness: RandomnessConfiguration::current(),
            serialization: SerializationConfiguration::current(),
        }
    }

    /// Serialize this profile with its canonical version-1 Borsh encoding.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("execution profile Borsh serialization is infallible")
    }

    /// Hash this profile's canonical versioned Borsh representation.
    #[must_use]
    pub fn hash(&self) -> ExecutionProfileHash {
        ExecutionProfileHash::of_bytes(&self.canonical_bytes())
    }
}

impl Default for ExecutionProfile {
    fn default() -> Self {
        Self::current()
    }
}

/// A 32-byte blake3 hash of an [`ExecutionProfile`] canonical Borsh value.
#[derive(
    BorshSerialize,
    BorshDeserialize,
    valuable::Valuable,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
)]
pub struct ExecutionProfileHash(pub [u8; 32]);

impl ExecutionProfileHash {
    /// Hash canonical profile bytes with blake3.
    #[must_use]
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    /// Borrow the raw hash bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl serde::Serialize for ExecutionProfileHash {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&hex::encode(self.0))
    }
}

impl<'de> serde::Deserialize<'de> for ExecutionProfileHash {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <String as serde::Deserialize>::deserialize(deserializer)?;
        crate::id::parse_id_hex(&value)
            .map(Self)
            .map_err(|error| serde::de::Error::custom(error.to_string()))
    }
}

impl schemars::JsonSchema for ExecutionProfileHash {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("ExecutionProfileHash")
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^[0-9a-f]{64}$"
        })
    }
}

impl std::fmt::Display for ExecutionProfileHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_changes<F>(mut change: F)
    where
        F: FnMut(&mut ExecutionProfile),
    {
        let profile = ExecutionProfile::current();
        let before = profile.hash();
        let mut changed = profile.clone();
        change(&mut changed);
        assert_ne!(before, changed.hash());
    }

    #[test]
    fn profile_hash_binds_every_execution_dimension() {
        assert_changes(|profile| profile.version += 1);
        assert_changes(|profile| profile.abi_version += 1);
        assert_changes(|profile| profile.semantics_version += 1);
        assert_changes(|profile| profile.engine_id.push('x'));
        assert_changes(|profile| profile.wasm_features.simd = true);
        assert_changes(|profile| profile.import_semantics.host_module.push('x'));
        assert_changes(|profile| profile.limits.max_memory_bytes += 1);
        assert_changes(|profile| profile.fuel.per_call += 1);
        assert_changes(|profile| profile.randomness.record_draws = false);
        assert_changes(|profile| {
            profile.serialization.agent_values = SerializationFormat::BorshV1;
        });
    }

    #[test]
    fn current_profile_carries_all_required_policy() {
        let profile = ExecutionProfile::current();
        assert_eq!(profile.version, EXECUTION_PROFILE_VERSION);
        assert_eq!(profile.abi_version, ABI_VERSION);
        assert_eq!(profile.semantics_version, EXECUTION_SEMANTICS_VERSION);
        assert_eq!(profile.engine_id, EXECUTION_ENGINE_ID);
        assert_eq!(
            profile.limits.max_call_envelope_bytes,
            MAX_CALL_ENVELOPE_BYTES as u64
        );
        assert_eq!(
            profile.limits.max_table_elements,
            MAX_WASM_TABLE_ELEMENTS as u64
        );
        assert_eq!(profile.limits.max_instances, MAX_WASM_INSTANCES as u64);
        assert_eq!(profile.limits.max_tables, MAX_WASM_TABLES as u64);
        assert_eq!(profile.limits.max_memories, MAX_WASM_MEMORIES as u64);
        assert!(profile.wasm_features.canonicalize_nan);
        assert_eq!(profile.import_semantics.host_module, crate::HOST_MODULE);
        assert!(profile.fuel.enabled);
        assert_eq!(
            profile.randomness.source,
            RandomnessSource::RecordedReplayable
        );
        assert_eq!(profile.serialization.schema, SchemaFormat::Draft202012);
    }
}
