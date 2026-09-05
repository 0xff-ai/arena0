//! Wasmtime ownership for one fresh guest invocation.

mod entropy;
mod imports;
mod instance;
mod memory;
mod runtime;

use std::sync::Arc;

use arena0_program::{
    ExecutionProfile, LocalStateBytes, MAX_WASM_STACK_BYTES, ProgramHash, SharedStateBytes,
};
use arena0_protocol::Effect;
use moka::sync::Cache;
use wasmtime::{Engine, Instance, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};

use crate::{Program, SandboxError, validation};
use entropy::Entropy;

const STACK_LIMIT: usize = MAX_WASM_STACK_BYTES;
const ADMISSION_CACHE_CAPACITY: u64 = 32;

/// The export currently being executed. Host imports use this to enforce
/// capability-specific read/write boundaries in addition to the guest's typed
/// context API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallKind {
    Metadata,
    Initialize,
    Shared,
    Local,
    Writer,
    Query,
    View,
    Outcome,
}

impl CallKind {
    /// Whether this export may request any host effect or diagnostic log.
    pub(crate) const fn allows_effects(self) -> bool {
        matches!(self, Self::Shared | Self::Local)
    }

    pub(crate) const fn allows_random(self) -> bool {
        matches!(self, Self::Local)
    }

    pub(crate) const fn allows_local_effects(self) -> bool {
        matches!(self, Self::Local)
    }

    /// Whether this call kind may record the given protocol effect.
    pub(crate) fn allows_effect(&self, effect: &Effect) -> bool {
        match self {
            Self::Local => true,
            Self::Shared => matches!(
                effect,
                Effect::SessionEnd { .. } | Effect::SessionAbort { .. } | Effect::Fail { .. }
            ),
            Self::Metadata
            | Self::Initialize
            | Self::Writer
            | Self::Query
            | Self::View
            | Self::Outcome => false,
        }
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
    pub lifecycle: arena0_protocol::Lifecycle,
    pub logs: Vec<(String, String)>,
    pub effect_queue: Vec<Effect>,
    pub next_continuation_tag: Option<u32>,
    pub entropy: Entropy,
    pub ledger: ResourceLedger,
    pub profile: ExecutionProfile,
    pub callout_inputs: Vec<arena0_program::JsonSchemaDocument>,
}

impl HostState {
    pub(crate) fn new(
        profile: ExecutionProfile,
        call_kind: CallKind,
        lifecycle: arena0_protocol::Lifecycle,
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
            lifecycle,
            logs: Vec::new(),
            effect_queue: Vec::new(),
            next_continuation_tag: None,
            entropy,
            ledger: ResourceLedger::new(),
            profile,
            callout_inputs,
        }
    }
}

/// Configured deterministic Wasmtime engine.
pub struct WasmtimeEngine {
    pub(crate) engine: Engine,
    pub(crate) profile: ExecutionProfile,
    pub(crate) admitted: Cache<ProgramHash, Arc<AdmittedProgram>>,
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
pub struct AdmittedProgram {
    pub(crate) engine: Engine,
    pub(crate) module: Module,
    pub(crate) program: Program,
    pub(crate) profile: ExecutionProfile,
    pub(crate) state_max_bytes: usize,
}

impl std::fmt::Debug for AdmittedProgram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdmittedProgram")
            .field("hash", &self.program.hash())
            .field("profile_hash", &self.profile.hash())
            .finish()
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
    pub lifecycle: arena0_protocol::Lifecycle,
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
        lifecycle,
        random_replay,
    } = config;
    let mut store = Store::new(
        engine,
        HostState::new(
            profile.clone(),
            call_kind,
            lifecycle,
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
    Ok(CallInstance { store, instance })
}

/// Drain host-owned effects and observations after a typed guest output has
/// been decoded. State and projections stay in their call-specific ABI values.
pub(crate) fn complete_observations(
    state: &mut HostState,
    fuel_used: u64,
) -> Result<crate::CallObservations, SandboxError> {
    Ok(crate::CallObservations {
        effects: std::mem::take(&mut state.effect_queue),
        fuel_used,
        random_draws: state
            .entropy
            .finish()
            .map_err(|error| SandboxError::dispatch_failed(error.to_string()))?,
        logs: std::mem::take(&mut state.logs),
    })
}

/// Check explicit state bytes against their domain bounds.
pub(crate) fn validate_state_bytes(
    shared: &SharedStateBytes,
    local: &LocalStateBytes,
    profile: &ExecutionProfile,
    schema_max_bytes: Option<usize>,
) -> Result<(), SandboxError> {
    let shared_max = schema_max_bytes
        .unwrap_or(usize::MAX)
        .min(profile.limits.max_shared_state_bytes as usize);
    if shared.len() > shared_max {
        return Err(SandboxError::MemoryLimitExceeded(shared.len() as u64));
    }
    if local.len() > profile.limits.max_local_state_bytes as usize {
        return Err(SandboxError::MemoryLimitExceeded(local.len() as u64));
    }
    Ok(())
}

pub(crate) fn max_output(profile: &ExecutionProfile) -> usize {
    profile.limits.max_output_bytes as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_kinds_expose_only_their_declared_effect_surface() {
        assert!(!CallKind::Initialize.allows_effects());
        assert!(!CallKind::Writer.allows_effects());
        assert!(!CallKind::Query.allows_effects());
        assert!(!CallKind::View.allows_effects());
        assert!(!CallKind::Outcome.allows_effects());
        assert!(CallKind::Shared.allows_effects());
        assert!(CallKind::Local.allows_effects());
        assert!(CallKind::Local.allows_random());
        assert!(!CallKind::Shared.allows_random());
        assert!(CallKind::Shared.allows_effect(&Effect::SessionEnd { outcome: vec![] }));
        assert!(!CallKind::Shared.allows_effect(&Effect::Broadcast { data: vec![] }));
        assert!(!CallKind::Initialize.allows_effect(&Effect::Fail {
            reason: String::new(),
        }));
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
}
