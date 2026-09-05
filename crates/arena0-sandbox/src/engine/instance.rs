//! Program loading and immutable admitted-code ownership.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use arena0_program::ExecutionProfile;
use arena0_protocol::Lifecycle;
use wasmtime::{Linker, Module, Store};

use super::imports::{register_always_available, register_metadata_imports};
use super::{CallKind, HostState, InstanceConfig, instantiate_module};
use crate::validation;
use crate::{Program, SandboxError};

impl super::WasmtimeEngine {
    /// Construct the deterministic Wasmtime engine and its profile.
    pub fn new() -> Result<Self, SandboxError> {
        Self::configured(None)
    }

    /// Construct an engine backed by Wasmtime's disk-persistent compilation cache.
    pub fn new_persistent(cache_dir: &Path) -> Result<Self, SandboxError> {
        let mut cache_config = wasmtime::CacheConfig::new();
        cache_config.with_directory(cache_dir);
        let cache = wasmtime::Cache::new(cache_config)
            .map_err(|error| SandboxError::CacheUnavailable(error.to_string()))?;
        Self::configured(Some(cache))
    }

    fn configured(persistent_cache: Option<wasmtime::Cache>) -> Result<Self, SandboxError> {
        let mut config = wasmtime::Config::new();
        config.cranelift_nan_canonicalization(true);
        config.max_wasm_stack(super::STACK_LIMIT);
        config.consume_fuel(true);
        config.wasm_simd(false);
        config.wasm_relaxed_simd(false);
        config.wasm_multi_memory(false);
        config.wasm_memory64(false);
        config.wasm_tail_call(false);
        config.cache(persistent_cache.clone());
        let engine = wasmtime::Engine::new(&config)
            .map_err(|error| SandboxError::compilation_failed(error.to_string()))?;
        Ok(Self {
            engine,
            profile: ExecutionProfile::current(),
            admitted: moka::sync::Cache::new(super::ADMISSION_CACHE_CAPACITY),
            persistent_cache,
        })
    }

    /// The exact execution profile used by this engine.
    #[must_use]
    pub fn profile(&self) -> &ExecutionProfile {
        &self.profile
    }

    /// Compile and validate a completed program artifact.
    pub fn admit(&self, program: &Program) -> Result<Arc<super::AdmittedProgram>, SandboxError> {
        let performance_enabled = tracing::enabled!(
            target: "arena0::performance",
            tracing::Level::DEBUG
        );
        let started = performance_enabled.then(Instant::now);
        let encoded_size = performance_enabled.then_some(program.bytes().len());
        let persistent_hits_before = performance_enabled
            .then(|| {
                self.persistent_cache
                    .as_ref()
                    .map(wasmtime::Cache::cache_hits)
            })
            .flatten();
        let admitted = self
            .admitted
            .entry(program.hash())
            .or_try_insert_with(|| self.compile(program));

        if let (Some(started), Some(encoded_size)) = (started, encoded_size) {
            let elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
            match &admitted {
                Ok(entry) => {
                    let persistent_hit = persistent_hits_before.is_some_and(|before| {
                        self.persistent_cache
                            .as_ref()
                            .is_some_and(|cache| cache.cache_hits() > before)
                    });
                    let result_class = match (entry.is_fresh(), persistent_hit) {
                        (false, _) => "cached",
                        (true, true) => "persistent_cached",
                        (true, false) => "compiled",
                    };
                    tracing::debug!(
                        target: "arena0::performance",
                        operation = "program_admission",
                        version = arena0_program::ABI_VERSION,
                        encoded_size,
                        success = true,
                        result_class,
                        elapsed_us,
                    );
                }
                Err(_) => tracing::debug!(
                    target: "arena0::performance",
                    operation = "program_admission",
                    version = arena0_program::ABI_VERSION,
                    encoded_size,
                    success = false,
                    result_class = "admission_error",
                    elapsed_us,
                ),
            }
        }

        admitted
            .map(|entry| entry.into_value())
            .map_err(Arc::unwrap_or_clone)
    }

    fn compile(&self, program: &Program) -> Result<Arc<super::AdmittedProgram>, SandboxError> {
        let module = Module::new(&self.engine, program.bytes())
            .map_err(|error| SandboxError::compilation_failed(error.to_string()))?;
        validation::validate_exports(&module)?;
        validation::validate_imports(&module, &program.definition().metadata)?;
        let _probe = instantiate_module(
            &self.engine,
            &module,
            InstanceConfig {
                metadata: &program.definition().metadata,
                schema: &program.definition().schema,
                profile: &self.profile,
                call_kind: CallKind::Metadata,
                lifecycle: Lifecycle::PreSession,
                random_replay: None,
            },
        )?;
        Ok(Arc::new(super::AdmittedProgram {
            engine: self.engine.clone(),
            module,
            program: program.clone(),
            profile: self.profile.clone(),
            state_max_bytes: program.definition().schema.state.max_bytes as usize,
        }))
    }

    /// Build a completed content-addressed program artifact. Existing
    /// metadata is parsed without execution; only section-less generated
    /// output uses the bounded metadata export probe.
    pub fn build_program(&self, wasm: &[u8]) -> Result<Program, SandboxError> {
        match Program::parse(wasm) {
            Ok(program) => Ok(program),
            Err(SandboxError::MissingMetadata) => {
                let definition = self.metadata_for_embedding(wasm)?;
                Program::embed(wasm, &definition)
            }
            Err(error) => Err(error),
        }
    }

    fn metadata_for_embedding(
        &self,
        binary: &[u8],
    ) -> Result<arena0_program::ProgramDefinition, SandboxError> {
        let module = Module::new(&self.engine, binary)
            .map_err(|error| SandboxError::compilation_failed(error.to_string()))?;
        validation::validate_exports(&module)?;
        let mut store = Store::new(
            &self.engine,
            HostState::new(
                self.profile.clone(),
                CallKind::Metadata,
                Lifecycle::PreSession,
                None,
                Vec::new(),
            ),
        );
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(self.profile.fuel.per_call)
            .map_err(|error| SandboxError::instantiation_failed(error.to_string()))?;
        let mut linker = Linker::new(&self.engine);
        register_always_available(&mut linker)?;
        register_metadata_imports(&mut linker)?;
        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|error| SandboxError::instantiation_failed(error.to_string()))?;
        validation::check_abi_version(&mut store, &instance)?;
        let (ptr, len) = {
            let mut guest = super::memory::Guest::new(&mut store, &instance);
            guest.call_metadata()?
        };
        if len as usize > self.profile.limits.max_metadata_bytes as usize {
            return Err(SandboxError::InvalidMetadata(format!(
                "metadata output exceeds {} bytes",
                self.profile.limits.max_metadata_bytes
            )));
        }
        let bytes = {
            let mut guest = super::memory::Guest::new(&mut store, &instance);
            let bytes = guest.read_mem(ptr, len)?;
            guest.dealloc(ptr, len)?;
            bytes
        };
        let definition = arena0_program::ProgramDefinition::decode(&bytes)
            .map_err(|error| SandboxError::InvalidMetadata(error.to_string()))?;
        validation::validate_program_definition(&definition)?;
        validation::validate_imports(&module, &definition.metadata)?;
        Ok(definition)
    }
}

impl super::AdmittedProgram {
    /// The exact parsed artifact that passed admission.
    #[must_use]
    pub fn program(&self) -> &Program {
        &self.program
    }

    /// The profile whose hash must be bound by protocol activation.
    #[must_use]
    pub fn profile(&self) -> &ExecutionProfile {
        &self.profile
    }
}
