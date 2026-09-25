//! Wasmtime ownership for resident dispatches and fresh projections.

mod entropy;
mod imports;
mod instance;
mod memory;
mod runtime;

pub use runtime::ProgramInstance;

use std::sync::Arc;

use arena0_program::{
    CalloutRequest, ExecutionProfile, LocalStateBytes, MAX_WASM_STACK_BYTES, ProgramHash,
    SharedStateBytes,
};
use arena0_protocol::Effect;
use moka::sync::Cache;
use wasmtime::{Engine, Instance, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};

use crate::{Program, SandboxError, validation};
use entropy::Entropy;

const STACK_LIMIT: usize = MAX_WASM_STACK_BYTES;
const PROGRAM_CACHE_CAPACITY: u64 = 32;

/// The export currently being executed. Host imports use this to enforce
/// capability-specific read/write boundaries in addition to the guest's typed
/// context API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallKind {
    Prepare,
    Metadata,
    Initialize,
    Dispatch,
    Writer,
    Query,
    View,
    Outcome,
}

impl CallKind {
    /// Whether this export may request any host effect or diagnostic log.
    pub(crate) const fn allows_effects(self) -> bool {
        matches!(self, Self::Dispatch)
    }

    pub(crate) const fn allows_state_io(self) -> bool {
        matches!(self, Self::Dispatch)
    }
}

/// Per-call host resource ledger. Every host allocation or import is charged
/// before work or memory growth occurs.
#[derive(Debug)]
pub(crate) struct ResourceLedger {
    pub host_calls: u64,
    pub host_bytes: u64,
    pub log_bytes: u64,
    pub log_entries: u64,
    pub effect_bytes: u64,
    pub effect_entries: u64,
    pub random_draws: u64,
}

impl ResourceLedger {
    fn new() -> Self {
        Self {
            host_calls: 0,
            host_bytes: 0,
            log_bytes: 0,
            log_entries: 0,
            effect_bytes: 0,
            effect_entries: 0,
            random_draws: 0,
        }
    }

    pub(crate) fn host_call(&mut self, max: u64) -> Result<(), SandboxError> {
        self.host_calls = self.host_calls.saturating_add(1);
        if self.host_calls > max {
            return Err(SandboxError::HostCallLimitExceeded {
                count: self.host_calls,
                max,
            });
        }
        Ok(())
    }

    pub(crate) fn copy_bytes(&mut self, bytes: usize, max: u64) -> Result<(), SandboxError> {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.host_bytes = self.host_bytes.saturating_add(bytes);
        if self.host_bytes > max {
            return Err(SandboxError::InputLimitExceeded(format!(
                "host copied {} bytes; maximum is {max}",
                self.host_bytes
            )));
        }
        Ok(())
    }

    pub(crate) fn log(
        &mut self,
        bytes: usize,
        max_bytes: u64,
        max_entries: u64,
    ) -> Result<(), SandboxError> {
        self.log_entries = self.log_entries.saturating_add(1);
        self.log_bytes = self
            .log_bytes
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        if self.log_entries > max_entries {
            return Err(SandboxError::LogLimitExceeded {
                actual: self.log_entries,
                max: max_entries,
            });
        }
        if self.log_bytes > max_bytes {
            return Err(SandboxError::LogLimitExceeded {
                actual: self.log_bytes,
                max: max_bytes,
            });
        }
        Ok(())
    }

    pub(crate) fn effect(
        &mut self,
        bytes: usize,
        max_bytes: u64,
        max_entries: u64,
    ) -> Result<(), SandboxError> {
        self.effect_entries = self.effect_entries.saturating_add(1);
        self.effect_bytes = self
            .effect_bytes
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        if self.effect_entries > max_entries {
            return Err(SandboxError::IntentOverflow {
                count: self.effect_entries.min(u64::from(u32::MAX)) as u32,
                max: max_entries.min(u64::from(u32::MAX)) as u32,
            });
        }
        if self.effect_bytes > max_bytes {
            return Err(SandboxError::EffectBytesLimitExceeded {
                actual: self.effect_bytes,
                max: max_bytes,
            });
        }
        Ok(())
    }

    pub(crate) fn random(&mut self, max: u64) -> Result<(), SandboxError> {
        self.random_draws = self.random_draws.saturating_add(1);
        if self.random_draws > max {
            return Err(SandboxError::HostCallLimitExceeded {
                count: self.random_draws,
                max,
            });
        }
        Ok(())
    }
}

/// Mutable state threaded through `wasmtime::Caller` for one call only.
#[derive(Debug)]
pub(crate) struct HostState {
    pub limits: StoreLimits,
    pub call_kind: CallKind,
    pub dispatch: crate::call::DispatchKind,
    /// Committed outgoing messages before this dispatch began.
    pub outgoing_len: usize,
    pub logs: Vec<(String, String)>,
    pub effect_queue: Vec<Effect>,
    pub entropy: Entropy,
    pub ledger: ResourceLedger,
    pub profile: ExecutionProfile,
    pub callout_inputs: Vec<arena0_program::JsonSchemaDocument>,
    pub signer: crate::signing::SignerSlot,
}

impl HostState {
    pub(crate) fn new(
        profile: ExecutionProfile,
        call_kind: CallKind,
        dispatch: crate::call::DispatchKind,
        random_replay: Option<&[Vec<u8>]>,
        callout_inputs: Vec<arena0_program::JsonSchemaDocument>,
    ) -> Self {
        let mut entropy = Entropy::live();
        if let Some(draws) = random_replay {
            entropy.set_replay(draws.to_vec());
        }
        Self {
            limits: StoreLimitsBuilder::new()
                .memory_size(profile.limits.max_memory_bytes as usize)
                .table_elements(profile.limits.max_table_elements as usize)
                .instances(profile.limits.max_instances as usize)
                .tables(profile.limits.max_tables as usize)
                .memories(profile.limits.max_memories as usize)
                .build(),
            call_kind,
            dispatch,
            outgoing_len: 0,
            logs: Vec::new(),
            effect_queue: Vec::new(),
            entropy,
            ledger: ResourceLedger::new(),
            profile,
            callout_inputs,
            signer: crate::signing::SignerSlot::default(),
        }
    }

    pub(crate) fn finish_observations(
        &mut self,
        fuel_used: u64,
    ) -> Result<crate::CallObservations, SandboxError> {
        Ok(crate::CallObservations {
            effects: std::mem::take(&mut self.effect_queue),
            fuel_used,
            random_draws: self
                .entropy
                .finish()
                .map_err(|error| SandboxError::dispatch_failed(error.to_string()))?,
            logs: std::mem::take(&mut self.logs),
        })
    }

    /// Clear every per-dispatch observation and reset deterministic host
    /// accounting before a resident instance is re-entered.
    pub(crate) fn reset_for_dispatch(
        &mut self,
        dispatch: crate::call::DispatchKind,
        outgoing_len: usize,
        random_replay: Option<&[Vec<u8>]>,
    ) {
        self.call_kind = CallKind::Dispatch;
        self.dispatch = dispatch;
        self.outgoing_len = outgoing_len;
        self.logs.clear();
        self.effect_queue.clear();
        self.ledger = ResourceLedger::new();
        self.entropy.reset();
        self.signer.clear();
        if let Some(draws) = random_replay {
            self.entropy.set_replay(draws.to_vec());
        }
    }

    /// Clear setup observations while retaining the call kind selected for a
    /// fresh operation. Preparation is a bootstrap boundary, not a dispatch;
    /// it must neither consume operation fuel nor leave allocator accounting
    /// in the subsequent call.
    pub(crate) fn reset_after_prepare(
        &mut self,
        call_kind: CallKind,
        random_replay: Option<&[Vec<u8>]>,
    ) {
        self.call_kind = call_kind;
        self.logs.clear();
        self.effect_queue.clear();
        self.ledger = ResourceLedger::new();
        self.entropy.reset();
        self.signer.clear();
        if let Some(draws) = random_replay {
            self.entropy.set_replay(draws.to_vec());
        }
    }
}

/// Configured deterministic Wasmtime engine.
pub struct WasmtimeEngine {
    pub(crate) engine: Engine,
    pub(crate) profile: ExecutionProfile,
    pub(crate) loaded: Cache<ProgramHash, Arc<LoadedProgram>>,
    pub(crate) persistent_cache: Option<wasmtime::Cache>,
}

impl std::fmt::Debug for WasmtimeEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmtimeEngine")
            .field("profile_hash", &self.profile.hash())
            .field("persistent_cache", &self.persistent_cache.is_some())
            .finish_non_exhaustive()
    }
}

/// Immutable program after metadata, exports, imports, ABI, and module
/// compilation have all been checked.
pub struct LoadedProgram {
    pub(crate) engine: Engine,
    pub(crate) module: Module,
    pub(crate) program: Program,
    pub(crate) profile: ExecutionProfile,
    pub(crate) state_max_bytes: usize,
}

impl std::fmt::Debug for LoadedProgram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedProgram")
            .field("hash", &self.program.hash())
            .field("profile_hash", &self.profile.hash())
            .finish()
    }
}

impl LoadedProgram {
    pub(crate) fn validate_shared_state(
        &self,
        shared: &SharedStateBytes,
    ) -> Result<(), SandboxError> {
        let shared_max = self
            .state_max_bytes
            .min(self.profile.limits.max_shared_state_bytes as usize);
        if shared.len() > shared_max {
            return Err(SandboxError::MemoryLimitExceeded(shared.len() as u64));
        }
        Ok(())
    }

    pub(crate) fn validate_state(
        &self,
        shared: &SharedStateBytes,
        local: &LocalStateBytes,
    ) -> Result<(), SandboxError> {
        self.validate_shared_state(shared)?;
        if local.len() > self.profile.limits.max_local_state_bytes as usize {
            return Err(SandboxError::MemoryLimitExceeded(local.len() as u64));
        }
        Ok(())
    }
}

/// A fresh Wasmtime store and instance. This value never escapes one call.
pub(crate) struct CallInstance {
    pub store: Store<HostState>,
    pub instance: Instance,
}

/// Immutable and per-call inputs used to create one fresh instance.
pub(crate) struct InstanceConfig<'a> {
    pub metadata: &'a arena0_program::ProgramMetadata,
    pub schema: &'a arena0_program::ProgramSchema,
    pub profile: &'a ExecutionProfile,
    pub call_kind: CallKind,
    pub dispatch: crate::call::DispatchKind,
    pub random_replay: Option<&'a [Vec<u8>]>,
}

pub(crate) fn instantiate_module(
    engine: &Engine,
    module: &Module,
    config: InstanceConfig<'_>,
) -> Result<CallInstance, SandboxError> {
    let InstanceConfig {
        metadata,
        schema,
        profile,
        call_kind,
        dispatch,
        random_replay,
    } = config;
    let mut store = Store::new(
        engine,
        HostState::new(
            profile.clone(),
            CallKind::Prepare,
            dispatch,
            random_replay,
            schema.callouts.iter().map(|c| c.input.clone()).collect(),
        ),
    );
    store.limiter(|state| &mut state.limits);
    store
        .set_fuel(profile.fuel.per_call)
        .map_err(|e| SandboxError::instantiation_failed(e.to_string()))?;
    let mut linker = Linker::new(engine);
    imports::register_always_available(&mut linker)?;
    imports::register_capability_imports(&mut linker, &metadata.capabilities)?;
    let instance = linker
        .instantiate(&mut store, module)
        .map_err(|e| SandboxError::instantiation_failed(e.to_string()))?;
    validation::check_abi_version(&mut store, &instance)?;
    instance
        .get_memory(&mut store, "memory")
        .ok_or_else(|| SandboxError::instantiation_failed("no 'memory' export"))?;
    {
        let mut guest = memory::Guest::new(&mut store, &instance);
        guest
            .prepare()
            .map_err(|error| SandboxError::instantiation_failed(error.to_string()))?;
    }
    store
        .data_mut()
        .reset_after_prepare(call_kind, random_replay);
    store
        .set_fuel(profile.fuel.per_call)
        .map_err(|error| SandboxError::instantiation_failed(error.to_string()))?;
    Ok(CallInstance { store, instance })
}

pub(crate) fn max_output(profile: &ExecutionProfile) -> usize {
    profile.limits.max_output_bytes as usize
}

/// Validate one derived callout context against the program's declared input
/// schema for that callout index. The index and JSON shape are guest-produced,
/// so a mismatch is a dispatch failure rather than an agent rejection.
pub(crate) fn validate_callout_context(
    callout_inputs: &[arena0_program::JsonSchemaDocument],
    callout: &CalloutRequest,
) -> Result<(), SandboxError> {
    let schema = callout_inputs
        .get(callout.callout_index as usize)
        .ok_or_else(|| SandboxError::dispatch_failed("unknown callout schema index"))?;
    let value: serde_json::Value = serde_json::from_slice(&callout.context).map_err(|error| {
        SandboxError::dispatch_failed(format!("callout context is not JSON: {error}"))
    })?;
    let validator = jsonschema::validator_for(schema.as_value()).map_err(|error| {
        SandboxError::dispatch_failed(format!("invalid callout schema: {error}"))
    })?;
    validator
        .validate(&value)
        .map_err(|_| SandboxError::dispatch_failed("callout context schema validation failed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_callout_requires_a_declared_index_and_valid_context() {
        let schemas = vec![
            arena0_program::JsonSchemaDocument::new(serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object", "required": ["round"],
                "properties": { "round": { "type": "integer" } }
            }))
            .unwrap(),
        ];
        let mut request = CalloutRequest {
            callout_index: 0,
            context: br#"{"round":1}"#.to_vec(),
        };
        assert!(validate_callout_context(&schemas, &request).is_ok());
        request.callout_index = 1;
        assert!(validate_callout_context(&schemas, &request).is_err());
        request.callout_index = 0;
        request.context = br#"{"round":"private-value"}"#.to_vec();
        let error = validate_callout_context(&schemas, &request).unwrap_err();
        assert!(!error.to_string().contains("private-value"));
        request.context = b"{".to_vec();
        assert!(validate_callout_context(&schemas, &request).is_err());
    }

    #[test]
    fn call_kinds_expose_only_their_declared_effect_surface() {
        assert!(!CallKind::Prepare.allows_effects());
        assert!(!CallKind::Prepare.allows_state_io());
        assert!(!CallKind::Initialize.allows_effects());
        assert!(!CallKind::Writer.allows_effects());
        assert!(!CallKind::Query.allows_effects());
        assert!(!CallKind::View.allows_effects());
        assert!(!CallKind::Outcome.allows_effects());
        assert!(CallKind::Dispatch.allows_effects());
    }

    #[test]
    fn resource_ledger_enforces_each_cumulative_budget() {
        let mut ledger = ResourceLedger::new();
        assert!(ledger.host_call(1).is_ok());
        assert!(ledger.host_call(1).is_err());

        let mut ledger = ResourceLedger::new();
        assert!(ledger.copy_bytes(2, 2).is_ok());
        assert!(ledger.copy_bytes(1, 2).is_err());

        let mut ledger = ResourceLedger::new();
        assert!(ledger.effect(2, 2, 1).is_ok());
        assert!(ledger.effect(1, 2, 1).is_err());

        let mut ledger = ResourceLedger::new();
        assert!(ledger.log(2, 2, 1).is_ok());
        assert!(ledger.log(0, 2, 1).is_err());
    }

    #[test]
    fn host_copy_budget_admits_maximum_state_round_trip_and_envelope() {
        let mut ledger = ResourceLedger::new();
        for bytes in [
            arena0_program::MAX_SHARED_STATE_BYTES,
            arena0_program::MAX_LOCAL_STATE_BYTES,
            arena0_program::MAX_SHARED_STATE_BYTES,
            arena0_program::MAX_LOCAL_STATE_BYTES,
            arena0_program::MAX_INPUT_BYTES as usize,
            arena0_program::MAX_OUTPUT_BYTES as usize,
            arena0_program::MAX_EFFECT_BYTES as usize,
            arena0_program::MAX_EFFECT_BYTES as usize,
        ] {
            ledger
                .copy_bytes(bytes, arena0_program::MAX_HOST_BYTES as u64)
                .unwrap();
        }
        assert_eq!(ledger.host_bytes, 32 * 1024 * 1024);
        assert!(
            ledger
                .copy_bytes(1, arena0_program::MAX_HOST_BYTES as u64)
                .is_err()
        );
    }
}
