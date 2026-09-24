//! Resident dispatch and fresh projection operations for loaded programs.

use arena0_program::{
    CallStatus, DispatchInput, DispatchOutput, InitInput, LocalStateBytes, OutcomeInput,
    OutcomeOutput, QueryInput, QueryOutput, SharedStateBytes, ViewInput, ViewOutput, WriterInput,
    WriterOutput, abi,
};
use arena0_protocol::Lifecycle;
use borsh::{BorshDeserialize, BorshSerialize};
use wasmtime::{Global, Instance, Memory, Store, StoreLimitsBuilder, Val};

use super::memory::Guest;
use super::{CallInstance, CallKind, InstanceConfig, instantiate_module, max_output};
use crate::call::{DispatchCall, InitializeCall, OutcomeCall, QueryCall, ViewCall, WriterCall};
use crate::finalize::MUTABLE_GLOBAL_EXPORT_PREFIX;
use crate::{
    CallObservations, DispatchCallResult, GuestOutcomeResult, GuestProjectionResult,
    GuestWriterResult, InitializedState, SandboxError,
};

impl super::LoadedProgram {
    /// Execute initialization in a fresh guest instance.
    pub fn initialize(&self, call: InitializeCall) -> Result<InitializedState, SandboxError> {
        let input = call.into_input()?;
        let (output, observations) = self.invoke::<InitInput, arena0_program::InitializedState>(
            CallKind::Initialize,
            Lifecycle::PreSession,
            None,
            abi::exports::INITIALIZE,
            input,
        )?;
        self.initialized_state(output, observations)
    }

    /// Create the sole resident Wasm instance for one execution.
    pub fn resident(
        &self,
        shared: SharedStateBytes,
        local: LocalStateBytes,
    ) -> Result<ProgramInstance, SandboxError> {
        self.validate_state(&shared, &local)?;
        let mut instance = instantiate_module(
            &self.engine,
            &self.module,
            InstanceConfig {
                metadata: &self.program.definition().metadata,
                schema: &self.program.definition().schema,
                profile: &self.profile,
                call_kind: CallKind::Dispatch,
                lifecycle: Lifecycle::Active,
                random_replay: None,
            },
        )?;
        let work = instance
            .instance
            .get_memory(&mut instance.store, "memory")
            .ok_or_else(|| SandboxError::instantiation_failed("no work memory export"))?;
        let shared_memory = instance
            .instance
            .get_memory(&mut instance.store, abi::exports::SHARED_MEMORY)
            .ok_or_else(|| SandboxError::instantiation_failed("no shared memory export"))?;
        let local_memory = instance
            .instance
            .get_memory(&mut instance.store, abi::exports::LOCAL_MEMORY)
            .ok_or_else(|| SandboxError::instantiation_failed("no local memory export"))?;
        let observed_work_bytes = work.data_size(&instance.store);
        let max_work_bytes = self
            .profile
            .limits
            .max_memory_bytes
            .min(arena0_program::MAX_WASM_MEMORY_BYTES as u64);
        let min_prepared = self.profile.limits.min_prepared_work_memory_bytes;
        if u64::try_from(observed_work_bytes).unwrap_or(u64::MAX) < min_prepared {
            return Err(SandboxError::instantiation_failed(format!(
                "arena0_prepare established {observed_work_bytes} work-memory bytes; minimum is {min_prepared}"
            )));
        }
        if u64::try_from(observed_work_bytes).unwrap_or(u64::MAX) > max_work_bytes {
            return Err(SandboxError::instantiation_failed(format!(
                "arena0_prepare established {observed_work_bytes} work-memory bytes; maximum is {max_work_bytes}"
            )));
        }
        instance.store.data_mut().limits = StoreLimitsBuilder::new()
            .memory_size(observed_work_bytes)
            .table_elements(self.profile.limits.max_table_elements as usize)
            .instances(self.profile.limits.max_instances as usize)
            .tables(self.profile.limits.max_tables as usize)
            .memories(self.profile.limits.max_memories as usize)
            .build();
        let baseline_work = work.data(&instance.store).to_vec();
        let mut globals = Vec::new();
        for export in self.module.exports() {
            if !export.name().starts_with(MUTABLE_GLOBAL_EXPORT_PREFIX)
                || export.ty().global().is_none()
            {
                continue;
            }
            let global = instance
                .instance
                .get_global(&mut instance.store, export.name())
                .ok_or_else(|| SandboxError::MissingExport(export.name().into()))?;
            let value = global.get(&mut instance.store);
            globals.push((global, value));
        }
        let mut resident = ProgramInstance {
            store: instance.store,
            instance: instance.instance,
            profile: self.profile.clone(),
            work,
            shared_memory,
            local_memory,
            baseline_work,
            globals,
            shared_max_bytes: self
                .state_max_bytes
                .min(self.profile.limits.max_shared_state_bytes as usize),
            committed_shared: SharedStateBytes::try_new(Vec::new())
                .map_err(|error| SandboxError::dispatch_failed(error.to_string()))?,
            committed_local: LocalStateBytes::try_new(Vec::new())
                .map_err(|error| SandboxError::dispatch_failed(error.to_string()))?,
        };
        resident.restore_payloads(shared, local)?;
        Ok(resident)
    }

    /// Execute one read-only query in a fresh guest instance.
    pub fn writer(&self, call: WriterCall) -> Result<GuestWriterResult, SandboxError> {
        let participant_count = call.session.len();
        let input = call.into_input();
        self.validate_shared_state(&input.shared)?;
        let (output, observations) = self.invoke::<WriterInput, WriterOutput>(
            CallKind::Writer,
            Lifecycle::Active,
            None,
            abi::exports::WRITER,
            input,
        )?;
        let fuel_used = observations.fuel_used;
        ensure_read_only(&observations, "writer")?;
        let writer = output.participant.map(arena0_protocol::Participant::new);
        if writer.is_some_and(|participant| participant.index() >= participant_count) {
            return Err(SandboxError::DispatchFailed(
                "writer is outside the committed ensemble".into(),
            ));
        }
        Ok(GuestWriterResult { writer, fuel_used })
    }

    /// Execute one read-only query in a fresh guest instance.
    pub fn query(&self, call: QueryCall) -> Result<GuestProjectionResult, SandboxError> {
        let schema = self
            .program
            .definition()
            .schema
            .queries
            .get(call.query_index as usize)
            .ok_or_else(|| {
                SandboxError::DeserializationFailed(format!(
                    "query index {} is not advertised",
                    call.query_index
                ))
            })?;
        let input = call.into_input()?;
        self.validate_shared_state(&input.shared)?;
        let query_index = input.query_index;
        let (output, observations) = self.invoke::<QueryInput, QueryOutput>(
            CallKind::Query,
            Lifecycle::Active,
            None,
            abi::exports::QUERY,
            input,
        )?;
        let fuel_used = observations.fuel_used;
        ensure_read_only(&observations, "query")?;
        if output.query_index != query_index {
            return Err(SandboxError::DispatchFailed(
                "query output index does not match input".into(),
            ));
        }
        projection_result(
            output.json,
            fuel_used,
            max_output(&self.profile),
            Some(&schema.response),
        )
    }

    /// Execute one read-only viewport projection in a fresh guest instance.
    pub fn view(&self, call: ViewCall) -> Result<GuestProjectionResult, SandboxError> {
        let input = call.into_input()?;
        self.validate_shared_state(&input.shared)?;
        let (output, observations) = self.invoke::<ViewInput, ViewOutput>(
            CallKind::View,
            Lifecycle::Active,
            None,
            abi::exports::VIEW,
            input,
        )?;
        let fuel_used = observations.fuel_used;
        ensure_read_only(&observations, "view")?;
        projection_result(output.json, fuel_used, max_output(&self.profile), None)
    }

    /// Execute the pure terminal-outcome projection in a fresh guest instance.
    pub fn outcome(&self, call: OutcomeCall) -> Result<GuestOutcomeResult, SandboxError> {
        let input = call.into_input()?;
        self.validate_shared_state(&input.shared)?;
        let (output, observations) = self.invoke::<OutcomeInput, OutcomeOutput>(
            CallKind::Outcome,
            Lifecycle::Completed,
            None,
            abi::exports::OUTCOME,
            input,
        )?;
        let fuel_used = observations.fuel_used;
        ensure_read_only(&observations, "outcome")?;
        outcome_result(
            output,
            fuel_used,
            max_output(&self.profile),
            &self.program.definition().schema.outcome,
        )
    }

    fn invoke<I, O>(
        &self,
        kind: CallKind,
        lifecycle: Lifecycle,
        random_replay: Option<&[Vec<u8>]>,
        export: &str,
        input: I,
    ) -> Result<(O, CallObservations), SandboxError>
    where
        I: BorshSerialize,
        O: BorshDeserialize,
    {
        let bytes = borsh::to_vec(&input)
            .map_err(|error| SandboxError::SerializationFailed(error.to_string()))?;
        if bytes.len() > self.profile.limits.max_call_envelope_bytes as usize {
            return Err(SandboxError::InputLimitExceeded(format!(
                "encoded call input is {} bytes; maximum is {}",
                bytes.len(),
                self.profile.limits.max_call_envelope_bytes
            )));
        }
        let bytes_len = u32::try_from(bytes.len()).map_err(|_| {
            SandboxError::InputLimitExceeded("call input length overflows u32".into())
        })?;
        let mut instance = instantiate_module(
            &self.engine,
            &self.module,
            InstanceConfig {
                metadata: &self.program.definition().metadata,
                schema: &self.program.definition().schema,
                profile: &self.profile,
                call_kind: kind,
                lifecycle,
                random_replay,
            },
        )?;
        let input_ptr = {
            let mut guest = Guest::new(&mut instance.store, &instance.instance);
            let ptr = guest.alloc(bytes_len)?;
            guest.write_mem_charged(ptr, &bytes, self.profile.limits.max_host_bytes)?;
            ptr
        };
        let returned = {
            let mut guest = Guest::new(&mut instance.store, &instance.instance);
            guest.call_pair_return(export, input_ptr, bytes_len)?
        };
        let output = self.decode_result::<O>(&mut instance, returned.0, returned.1)?;
        {
            let mut guest = Guest::new(&mut instance.store, &instance.instance);
            guest.zero_mem(input_ptr, bytes_len)?;
            guest.dealloc(input_ptr, bytes_len)?;
        }
        let fuel_used = self.profile.fuel.per_call.saturating_sub(
            instance
                .store
                .get_fuel()
                .unwrap_or(self.profile.fuel.per_call),
        );
        let observations = instance.store.data_mut().finish_observations(fuel_used)?;
        Ok((output, observations))
    }

    fn decode_result<O: BorshDeserialize>(
        &self,
        instance: &mut CallInstance,
        ptr: u32,
        len: u32,
    ) -> Result<O, SandboxError> {
        if len as usize > self.profile.limits.max_call_envelope_bytes as usize {
            return Err(SandboxError::OutputLimitExceeded {
                size: len as u64,
                max: self.profile.limits.max_call_envelope_bytes,
            });
        }
        let bytes = {
            let mut guest = Guest::new(&mut instance.store, &instance.instance);
            let bytes = guest.read_mem_charged(ptr, len, self.profile.limits.max_host_bytes)?;
            guest.zero_mem(ptr, len)?;
            guest.dealloc(ptr, len)?;
            bytes
        };
        borsh::from_slice(&bytes)
            .map_err(|error| SandboxError::DeserializationFailed(error.to_string()))
    }
}

fn projection_result(
    output: Vec<u8>,
    fuel_used: u64,
    max_output_bytes: usize,
    schema: Option<&arena0_program::JsonSchemaDocument>,
) -> Result<GuestProjectionResult, SandboxError> {
    if output.len() > max_output_bytes {
        return Err(SandboxError::OutputLimitExceeded {
            size: output.len() as u64,
            max: max_output_bytes as u64,
        });
    }
    let output = arena0_program::JsonBytes::try_new(output)
        .map_err(|error| SandboxError::DeserializationFailed(error.to_string()))?;
    if let Some(schema) = schema {
        validate_json_schema(&output, schema)?;
    }
    Ok(GuestProjectionResult { output, fuel_used })
}

fn outcome_result(
    output: OutcomeOutput,
    fuel_used: u64,
    max_output_bytes: usize,
    schema: &arena0_program::JsonSchemaDocument,
) -> Result<GuestOutcomeResult, SandboxError> {
    let borsh = arena0_program::OutcomeBytes::try_new(output.borsh)
        .map_err(|error| SandboxError::DeserializationFailed(error.to_string()))?;
    let json = arena0_program::JsonBytes::try_new(output.json)
        .map_err(|error| SandboxError::DeserializationFailed(error.to_string()))?;
    validate_json_schema(&json, schema)?;
    if borsh.as_bytes().len() > max_output_bytes || json.as_bytes().len() > max_output_bytes {
        return Err(SandboxError::OutputLimitExceeded {
            size: borsh.as_bytes().len().max(json.as_bytes().len()) as u64,
            max: max_output_bytes as u64,
        });
    }
    Ok(GuestOutcomeResult {
        borsh,
        json,
        fuel_used,
    })
}

fn ensure_read_only(observations: &CallObservations, operation: &str) -> Result<(), SandboxError> {
    if observations.is_empty_except_fuel() {
        Ok(())
    } else {
        Err(SandboxError::ReadOnlyViolation {
            operation: operation.to_owned(),
        })
    }
}

/// One actor-owned Wasm instance. This type is intentionally non-cloneable:
/// the host must serialize dispatches through one owner and decide explicitly
/// when candidate state becomes committed.
pub struct ProgramInstance {
    pub(crate) store: Store<super::HostState>,
    pub(crate) instance: Instance,
    profile: arena0_program::ExecutionProfile,
    work: Memory,
    shared_memory: Memory,
    local_memory: Memory,
    baseline_work: Vec<u8>,
    globals: Vec<(Global, Val)>,
    shared_max_bytes: usize,
    committed_shared: SharedStateBytes,
    committed_local: LocalStateBytes,
}

impl std::fmt::Debug for ProgramInstance {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProgramInstance")
            .field("committed_shared_len", &self.committed_shared.len())
            .field("committed_local_len", &self.committed_local.len())
            .finish_non_exhaustive()
    }
}

impl ProgramInstance {
    /// Dispatch one event through the sole mutating guest export.
    pub fn dispatch(&mut self, call: DispatchCall) -> Result<DispatchCallResult, SandboxError> {
        let (input, random_replay, lifecycle) = call.into_input()?;
        self.dispatch_input(input, random_replay.as_ref(), lifecycle)
    }

    /// Replace the resident state with durable committed payloads during actor
    /// recovery. This also updates the rollback checkpoint.
    pub fn restore_payloads(
        &mut self,
        shared: SharedStateBytes,
        local: LocalStateBytes,
    ) -> Result<(), SandboxError> {
        self.validate_payloads(&shared, &local)?;
        write_state_payload(&mut self.store, self.shared_memory, shared.as_bytes())?;
        write_state_payload(&mut self.store, self.local_memory, local.as_bytes())?;
        self.reset_work_and_globals()?;
        self.committed_shared = shared;
        self.committed_local = local;
        Ok(())
    }

    /// Restore the last committed payloads after proposal freeze or a failed
    /// durable operation.
    pub fn restore_committed(&mut self) -> Result<(), SandboxError> {
        // The checkpoint is already validated when it is installed. Borrowing
        // it here avoids cloning each payload on every reject/failure path.
        write_state_payload(
            &mut self.store,
            self.shared_memory,
            self.committed_shared.as_bytes(),
        )?;
        write_state_payload(
            &mut self.store,
            self.local_memory,
            self.committed_local.as_bytes(),
        )?;
        self.reset_work_and_globals()
    }

    /// Promote the currently resident state images to the rollback checkpoint
    /// after the owning store transaction confirms commitment.
    pub fn commit_payloads(&mut self) -> Result<(SharedStateBytes, LocalStateBytes), SandboxError> {
        let (shared, local, _) = self.resident_payloads()?;
        self.committed_shared = shared.clone();
        self.committed_local = local.clone();
        Ok((shared, local))
    }

    /// Return the last state images known to be committed by the owner.
    #[must_use]
    pub fn committed_payloads(&self) -> (&SharedStateBytes, &LocalStateBytes) {
        (&self.committed_shared, &self.committed_local)
    }

    fn resident_payloads(
        &self,
    ) -> Result<(SharedStateBytes, LocalStateBytes, [u8; 32]), SandboxError> {
        let shared_view = canonical_state_view(
            &self.store,
            self.shared_memory,
            self.shared_max_bytes,
            "shared",
        )?;
        let local_view = canonical_state_view(
            &self.store,
            self.local_memory,
            self.profile.limits.max_local_state_bytes as usize,
            "local",
        )?;
        let shared_hash = arena0_protocol::StateHash::of(shared_view.image).0;
        let shared = SharedStateBytes::try_from_slice(shared_view.payload)
            .map_err(|error| SandboxError::dispatch_failed(error.to_string()))?;
        let local = LocalStateBytes::try_from_slice(local_view.payload)
            .map_err(|error| SandboxError::dispatch_failed(error.to_string()))?;
        Ok((shared, local, shared_hash))
    }

    fn rollback_error<T>(&mut self, error: SandboxError) -> Result<T, SandboxError> {
        let _ = self.rollback_after_failure();
        Err(error)
    }

    fn dispatch_input(
        &mut self,
        input: DispatchInput,
        random_replay: Option<&crate::call::RandomReplay>,
        lifecycle: Lifecycle,
    ) -> Result<DispatchCallResult, SandboxError> {
        let bytes = borsh::to_vec(&input)
            .map_err(|error| SandboxError::SerializationFailed(error.to_string()))?;
        if bytes.len() > self.profile.limits.max_call_envelope_bytes as usize {
            return Err(SandboxError::InputLimitExceeded(format!(
                "encoded dispatch input is {} bytes; maximum is {}",
                bytes.len(),
                self.profile.limits.max_call_envelope_bytes
            )));
        }
        let bytes_len = u32::try_from(bytes.len()).map_err(|_| {
            SandboxError::InputLimitExceeded("dispatch input length overflows u32".into())
        })?;
        if let Err(error) = self.reset_for_dispatch(lifecycle, random_replay) {
            return self.rollback_error(error);
        }
        let input_ptr_result = {
            let mut guest = Guest::new(&mut self.store, &self.instance);
            guest.alloc(bytes_len).and_then(|ptr| {
                guest
                    .write_mem_charged(ptr, &bytes, self.profile.limits.max_host_bytes)
                    .map(|()| ptr)
            })
        };
        let input_ptr = input_ptr_result.or_else(|error| self.rollback_error(error))?;
        let returned = {
            let mut guest = Guest::new(&mut self.store, &self.instance);
            guest.call_pair_return(abi::exports::DISPATCH, input_ptr, bytes_len)
        };
        let returned = returned.or_else(|error| self.rollback_error(error))?;
        let output = self
            .decode_resident_result(returned.0, returned.1)
            .or_else(|error| self.rollback_error(error))?;
        {
            let mut guest = Guest::new(&mut self.store, &self.instance);
            if let Err(error) = guest
                .zero_mem(input_ptr, bytes_len)
                .and_then(|()| guest.dealloc(input_ptr, bytes_len))
            {
                return self.rollback_error(error);
            }
        }
        let fuel_used = self
            .profile
            .fuel
            .per_call
            .saturating_sub(self.store.get_fuel().unwrap_or(self.profile.fuel.per_call));
        // A rejected handler has no state or observation commit semantics.
        // Restore before inspecting its candidate frames so malformed or
        // oversized guest writes cannot turn a normal rejection into a
        // sandbox error. Setup and dispatch observations are all provisional,
        // including replayable random bytes.
        if output.status == CallStatus::Rejected {
            self.rollback_after_failure()?;
            return Ok(DispatchCallResult {
                status: output.status,
                reason: output.reason,
                shared: self.committed_shared.clone(),
                local: self.committed_local.clone(),
                shared_hash: self.committed_shared_hash()?,
                observations: CallObservations {
                    effects: Vec::new(),
                    fuel_used,
                    random_draws: Vec::new(),
                    logs: Vec::new(),
                },
            });
        }

        let observations = self
            .store
            .data_mut()
            .finish_observations(fuel_used)
            .or_else(|error| self.rollback_error(error))?;

        // The state-validation boundary borrows the fixed memories directly:
        // one shared view supplies both the payload copy and its hash, and the
        // local view supplies only its payload copy. The entry reset above is
        // the sole successful work-memory/global reset boundary; failed calls
        // use rollback below, while the next dispatch deterministically resets
        // any successful call's temporary work state before re-entry.
        let (shared, local, shared_hash) = self
            .resident_payloads()
            .or_else(|error| self.rollback_error(error))?;
        Ok(DispatchCallResult {
            status: output.status,
            reason: output.reason,
            shared,
            local,
            shared_hash,
            observations,
        })
    }

    fn decode_resident_result(
        &mut self,
        ptr: u32,
        len: u32,
    ) -> Result<DispatchOutput, SandboxError> {
        if len as usize > self.profile.limits.max_call_envelope_bytes as usize {
            return Err(SandboxError::OutputLimitExceeded {
                size: len as u64,
                max: self.profile.limits.max_call_envelope_bytes,
            });
        }
        let bytes = {
            let mut guest = Guest::new(&mut self.store, &self.instance);
            let bytes = guest.read_mem_charged(ptr, len, self.profile.limits.max_host_bytes)?;
            guest.zero_mem(ptr, len)?;
            guest.dealloc(ptr, len)?;
            bytes
        };
        borsh::from_slice(&bytes)
            .map_err(|error| SandboxError::DeserializationFailed(error.to_string()))
    }

    fn validate_payloads(
        &self,
        shared: &SharedStateBytes,
        local: &LocalStateBytes,
    ) -> Result<(), SandboxError> {
        if shared.len() > self.shared_max_bytes {
            return Err(SandboxError::MemoryLimitExceeded(shared.len() as u64));
        }
        if local.len() > self.profile.limits.max_local_state_bytes as usize {
            return Err(SandboxError::MemoryLimitExceeded(local.len() as u64));
        }
        Ok(())
    }

    fn reset_for_dispatch(
        &mut self,
        lifecycle: Lifecycle,
        random_replay: Option<&crate::call::RandomReplay>,
    ) -> Result<(), SandboxError> {
        // Reset at entry so every guest call sees the same temporary-memory and
        // mutable-global baseline, including when a prior accepted result was
        // not yet committed by the owning store transaction.
        let reset_result = self.reset_work_and_globals();
        self.store.data_mut().reset_for_dispatch(
            lifecycle,
            random_replay.map(crate::call::RandomReplay::as_slice),
        );
        reset_result?;
        self.store
            .set_fuel(self.profile.fuel.per_call)
            .map_err(|error| SandboxError::dispatch_failed(error.to_string()))
    }

    fn reset_work_and_globals(&mut self) -> Result<(), SandboxError> {
        if self.work.data_size(&self.store) != self.baseline_work.len() {
            return Err(SandboxError::dispatch_failed(
                "resident work memory size changed",
            ));
        }
        self.work
            .data_mut(&mut self.store)
            .copy_from_slice(&self.baseline_work);
        for (global, value) in &self.globals {
            global
                .set(&mut self.store, *value)
                .map_err(|error| SandboxError::dispatch_failed(error.to_string()))?;
        }
        Ok(())
    }

    fn rollback_after_failure(&mut self) -> Result<(), SandboxError> {
        let restore_result = self.restore_committed();
        self.store
            .data_mut()
            .reset_for_dispatch(Lifecycle::Active, None);
        restore_result
    }

    fn committed_shared_hash(&self) -> Result<[u8; 32], SandboxError> {
        let view = canonical_state_view(
            &self.store,
            self.shared_memory,
            self.shared_max_bytes,
            "shared",
        )?;
        Ok(arena0_protocol::StateHash::of(view.image).0)
    }
}

/// Borrowed view of one validated fixed-width state memory.
///
/// The view keeps the complete canonical image for hashing and the bounded
/// payload range for the one unavoidable host-owned state copy. It never owns
/// guest memory and must not outlive the immutable store borrow used to create
/// it.
struct CanonicalStateView<'a> {
    image: &'a [u8],
    payload: &'a [u8],
}

fn canonical_state_view<'a>(
    store: &'a Store<super::HostState>,
    memory: Memory,
    max_payload: usize,
    label: &str,
) -> Result<CanonicalStateView<'a>, SandboxError> {
    let data = memory.data(store);
    let prefix = arena0_program::CANONICAL_STATE_PREFIX_BYTES;
    if data.len() < prefix {
        return Err(SandboxError::dispatch_failed(format!(
            "{label} memory is smaller than its length prefix"
        )));
    }
    if data.len() != arena0_program::CANONICAL_STATE_MEMORY_BYTES {
        return Err(SandboxError::dispatch_failed(format!(
            "{label} memory has {} bytes; expected {}",
            data.len(),
            arena0_program::CANONICAL_STATE_MEMORY_BYTES
        )));
    }
    let length = u32::from_le_bytes(data[..prefix].try_into().unwrap()) as usize;
    let end = prefix
        .checked_add(length)
        .ok_or_else(|| SandboxError::dispatch_failed(format!("{label} length overflow")))?;
    if end > data.len() {
        return Err(SandboxError::dispatch_failed(format!(
            "{label} length {length} exceeds {}-byte memory",
            data.len()
        )));
    }
    if data[end..].iter().any(|byte| *byte != 0) {
        return Err(SandboxError::dispatch_failed(format!(
            "{label} memory has non-zero bytes after its payload"
        )));
    }
    if length > max_payload {
        return Err(SandboxError::MemoryLimitExceeded(length as u64));
    }
    Ok(CanonicalStateView {
        image: data,
        payload: &data[prefix..end],
    })
}

fn write_state_payload(
    store: &mut Store<super::HostState>,
    memory: Memory,
    payload: &[u8],
) -> Result<(), SandboxError> {
    let prefix = arena0_program::CANONICAL_STATE_PREFIX_BYTES;
    let max_payload = arena0_program::CANONICAL_STATE_MEMORY_BYTES - prefix;
    if payload.len() > max_payload {
        return Err(SandboxError::dispatch_failed(format!(
            "state payload is {} bytes; canonical frame allows {max_payload}",
            payload.len()
        )));
    }
    let data = memory.data_mut(store);
    if data.len() != arena0_program::CANONICAL_STATE_MEMORY_BYTES {
        return Err(SandboxError::MemoryLimitExceeded(
            arena0_program::CANONICAL_STATE_MEMORY_BYTES as u64,
        ));
    }
    data.fill(0);
    let payload_len = u32::try_from(payload.len()).expect("canonical state length fits in u32");
    data[..prefix].copy_from_slice(&payload_len.to_le_bytes());
    let end = prefix + payload.len();
    data[prefix..end].copy_from_slice(payload);
    Ok(())
}

impl super::LoadedProgram {
    fn initialized_state(
        &self,
        output: arena0_program::InitializedState,
        observations: CallObservations,
    ) -> Result<InitializedState, SandboxError> {
        if !observations.is_empty_except_fuel() {
            return Err(SandboxError::DispatchFailed(
                "initialization must return state only".into(),
            ));
        }
        self.validate_state(&output.shared, &output.local)?;
        Ok(InitializedState {
            shared: output.shared,
            local: output.local,
            fuel_used: observations.fuel_used,
        })
    }
}

fn validate_json_schema(
    bytes: &arena0_program::JsonBytes,
    schema: &arena0_program::JsonSchemaDocument,
) -> Result<(), SandboxError> {
    let value: serde_json::Value = serde_json::from_slice(bytes.as_bytes())
        .map_err(|error| SandboxError::DeserializationFailed(error.to_string()))?;
    let validator = jsonschema::validator_for(schema.as_value())
        .map_err(|error| SandboxError::InvalidMetadata(error.to_string()))?;
    validator.validate(&value).map_err(|error| {
        SandboxError::DeserializationFailed(format!("JSON schema validation failed: {error}"))
    })
}

#[cfg(test)]
mod resident_runtime_tests {
    use super::*;
    use crate::{Program, WasmtimeEngine};

    use arena0_program::{
        Capability, JsonSchemaDocument, ProgramDefinition, ProgramMetadata, ProgramSchema,
        StateSchema,
    };
    use arena0_protocol::{Committed, Effect, Ensemble, Event, PeerId};

    fn unit_schema() -> JsonSchemaDocument {
        JsonSchemaDocument::new(serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "null"
        }))
        .unwrap()
    }

    fn definition(capabilities: Vec<Capability>) -> ProgramDefinition {
        let unit = unit_schema();
        ProgramDefinition {
            metadata: ProgramMetadata {
                name: "resident-fixture".into(),
                version: "1".into(),
                description: "resident dispatch fixture".into(),
                author: None,
                capabilities,
                display_name: "Resident fixture".into(),
                participants: arena0_program::ParticipantCount::Exact { count: 2 },
            },
            schema: ProgramSchema {
                state: StateSchema {
                    schema: unit.clone(),
                    max_bytes: 64,
                },
                callouts: Vec::new(),
                messages: Vec::new(),
                params: unit.clone(),
                queries: Vec::new(),
                outcome: unit,
            },
        }
    }

    fn wat_data(bytes: &[u8]) -> String {
        bytes
            .iter()
            .map(|byte| format!("\\{byte:02x}"))
            .collect::<Vec<_>>()
            .join("")
    }

    fn dispatch_module(body: &str, capabilities: Vec<Capability>, extra_imports: &str) -> Vec<u8> {
        let definition = definition(capabilities);
        let metadata_bytes = definition.encode().unwrap();
        let metadata = wat_data(&metadata_bytes);
        let output = wat_data(&[0, 0]);
        let initialize = wat_data(&[0; 8]);
        let wat = format!(
            r#"
            (module
              (import "arena0" "state_len" (func $state_len (param i32) (result i32)))
              (import "arena0" "state_read" (func $state_read (param i32 i32 i32)))
              (import "arena0" "state_write" (func $state_write (param i32 i32 i32)))
              {extra_imports}
              (memory (export "memory") 1)
              (global (export "arena0_abi_version") i32 (i32.const 22))
              (global $counter (mut i32) (i32.const 0))
              (data (i32.const 1024) "sh")
              (data (i32.const 1100) "effect")
              (data (i32.const 3000) "{initialize}")
              (data (i32.const 32768) "{output}")
              (data (i32.const 40000) "{metadata}")
              (func $pack (param $ptr i32) (param $len i32) (result i64)
                local.get $ptr
                i64.extend_i32_u
                i64.const 32
                i64.shl
                local.get $len
                i64.extend_i32_u
                i64.or)
              (func (export "arena0_alloc") (param i32) (result i32) i32.const 1048576)
              (func (export "arena0_dealloc") (param i32 i32))
              (func (export "arena0_prepare") (result i32)
                i32.const 895
                memory.grow
                drop
                i32.const 1)
              (func (export "arena0_initialize") (param i32 i32) (result i64)
                i32.const 3000
                i32.const 8
                call $pack)
              (func (export "arena0_dispatch") (param i32 i32) (result i64)
                {body})
              (func (export "arena0_writer") (param i32 i32) (result i64) i64.const 0)
              (func (export "arena0_query") (param i32 i32) (result i64) i64.const 0)
              (func (export "arena0_view") (param i32 i32) (result i64) i64.const 0)
              (func (export "arena0_outcome") (param i32 i32) (result i64) i64.const 0)
              (func (export "arena0_metadata") (result i64)
                i32.const 40000
                i32.const {metadata_len}
                call $pack))
            "#,
            body = body,
            extra_imports = extra_imports,
            initialize = initialize,
            output = output,
            metadata = metadata,
            metadata_len = metadata_bytes.len(),
        );
        wat::parse_str(wat).unwrap()
    }

    fn accepted_body() -> &'static str {
        r#"
          (local $next i32)
          global.get $counter
          i32.const 1
          i32.add
          local.tee $next
          global.set $counter
          i32.const 2000
          local.get $next
          i32.store8
          i32.const 0
          i32.const 1024
          i32.const 2
          call $state_write
          i32.const 1
          i32.const 2000
          i32.const 1
          call $state_write
          i32.const 0
          call $state_len
          drop
          i32.const 0
          i32.const 2200
          i32.const 2
          call $state_read
          i32.const 1100
          i32.const 6
          call $broadcast
          i32.const 32768
          i32.const 2
          call $pack
        "#
    }

    fn growth_after_prepare_body() -> &'static str {
        r#"
          i32.const 1
          memory.grow
          i32.const -1
          i32.ne
          if
            unreachable
          end
          i32.const 32768
          i32.const 2
          call $pack
        "#
    }

    fn oversized_shared_body() -> &'static str {
        r#"
          i32.const 0
          i32.const 1024
          i32.const 65
          call $state_write
          i32.const 1
          i32.const 2000
          i32.const 1
          call $state_write
          i32.const 32768
          i32.const 2
          call $pack
        "#
    }

    fn rejected_body() -> &'static str {
        r#"
          i32.const 0
          i32.const 1024
          i32.const 2
          call $state_write
          i32.const 1
          i32.const 1024
          i32.const 2
          call $state_write
          i32.const 32768
          i32.const 1
          i32.store8
          i32.const 32768
          i32.const 2
          call $pack
        "#
    }

    fn rejected_with_observations_body() -> &'static str {
        r#"
          i32.const 0
          i32.const 1024
          i32.const 2
          call $state_write
          i32.const 1
          i32.const 1024
          i32.const 2
          call $state_write
          i32.const 1100
          i32.const 6
          call $broadcast
          i32.const 1
          i32.const 1100
          i32.const 6
          call $log
          i32.const 2200
          i32.const 2
          call $random
          i32.const 5000
          i32.const 7
          i32.store8
          i32.const 5000
          i32.const 8
          i32.store8 offset=1
          i32.const 32768
          i32.const 1
          i32.store8
          i32.const 32768
          i32.const 2
          call $pack
        "#
    }

    fn call() -> DispatchCall {
        let peer = PeerId([1; 32]);
        let session = Ensemble::<Committed>::from_peers(vec![peer, PeerId([2; 32])]).unwrap();
        DispatchCall::new(peer, session, Event::React)
    }

    fn resident(body: &str, capabilities: Vec<Capability>, imports: &str) -> ProgramInstance {
        let engine = WasmtimeEngine::new().unwrap();
        let raw = dispatch_module(body, capabilities, imports);
        let program = engine.build_program(&raw).unwrap();
        engine
            .load(&program)
            .unwrap()
            .resident(
                arena0_program::SharedStateBytes::try_new(Vec::new()).unwrap(),
                arena0_program::LocalStateBytes::try_new(Vec::new()).unwrap(),
            )
            .unwrap()
    }

    fn finalized_tail_program() -> Program {
        let definition = definition(Vec::new());
        let wat = r#"
            (module
              (memory (export "memory") 1 1024)
              (memory (export "arena0_shared") 65 65)
              (memory (export "arena0_local") 65 65)
              (global (export "arena0_abi_version") i32 (i32.const 22))
              (data (i32.const 32768) "\00\00")
              (func $pack (param $ptr i32) (param $len i32) (result i64)
                local.get $ptr
                i64.extend_i32_u
                i64.const 32
                i64.shl
                local.get $len
                i64.extend_i32_u
                i64.or)
              (func (export "arena0_alloc") (param i32) (result i32) i32.const 1048576)
              (func (export "arena0_dealloc") (param i32 i32))
              (func (export "arena0_prepare") (result i32)
                i32.const 895
                memory.grow
                drop
                i32.const 1)
              (func (export "arena0_initialize") (param i32 i32) (result i64) i64.const 0)
              (func (export "arena0_dispatch") (param i32 i32) (result i64)
                i32.const 100
                i32.const 1
                i32.store8 1
                i32.const 32768
                i32.const 2
                call $pack)
              (func (export "arena0_writer") (param i32 i32) (result i64) i64.const 0)
              (func (export "arena0_query") (param i32 i32) (result i64) i64.const 0)
              (func (export "arena0_view") (param i32 i32) (result i64) i64.const 0)
              (func (export "arena0_outcome") (param i32 i32) (result i64) i64.const 0)
              (func (export "arena0_metadata") (result i64) i64.const 0))
        "#;
        Program::embed(&wat::parse_str(wat).unwrap(), &definition).unwrap()
    }

    fn finalized_allocator_failure_program(allocator_body: &str) -> Program {
        let definition = definition(Vec::new());
        let wat = format!(
            r#"
            (module
              (memory (export "memory") 1 1024)
              (memory (export "arena0_shared") 65 65)
              (memory (export "arena0_local") 65 65)
              (global (export "arena0_abi_version") i32 (i32.const 22))
              (data (i32.const 32768) "\00")
              (func (export "arena0_alloc") (param i32) (result i32)
                {allocator_body})
              (func (export "arena0_dealloc") (param i32 i32))
              (func (export "arena0_prepare") (result i32)
                i32.const 895
                memory.grow
                drop
                i32.const 1)
              (func (export "arena0_initialize") (param i32 i32) (result i64) i64.const 0)
              (func (export "arena0_dispatch") (param i32 i32) (result i64) i64.const 0)
              (func (export "arena0_writer") (param i32 i32) (result i64) i64.const 0)
              (func (export "arena0_query") (param i32 i32) (result i64) i64.const 0)
              (func (export "arena0_view") (param i32 i32) (result i64) i64.const 0)
              (func (export "arena0_outcome") (param i32 i32) (result i64) i64.const 0)
              (func (export "arena0_metadata") (result i64) i64.const 0))
            "#,
            allocator_body = allocator_body,
        );
        Program::embed(&wat::parse_str(&wat).unwrap(), &definition).unwrap()
    }

    fn allocator_failure_resident(allocator_body: &str) -> ProgramInstance {
        let engine = WasmtimeEngine::new().unwrap();
        let program = finalized_allocator_failure_program(allocator_body);
        engine
            .load(&program)
            .unwrap()
            .resident(
                arena0_program::SharedStateBytes::try_new(Vec::new()).unwrap(),
                arena0_program::LocalStateBytes::try_new(Vec::new()).unwrap(),
            )
            .unwrap()
    }

    #[test]
    fn finalizer_produces_exact_idempotent_three_memory_artifact() {
        let engine = WasmtimeEngine::new().unwrap();
        let raw = dispatch_module(
            accepted_body(),
            vec![Capability::Messaging],
            r#"(import "arena0" "broadcast" (func $broadcast (param i32 i32)))"#,
        );
        let program = engine.build_program(&raw).unwrap();
        let shape = crate::finalize::inspect(program.bytes()).unwrap();
        assert_eq!(shape.defined_memories.len(), 3);
        assert_eq!(
            shape.defined_memories,
            vec![(1, Some(1024)), (65, Some(65)), (65, Some(65))]
        );
        assert_eq!(
            shape
                .exports
                .get(arena0_program::abi::exports::SHARED_MEMORY),
            Some(&(wasmparser::ExternalKind::Memory, 1))
        );
        assert_eq!(
            shape
                .exports
                .get(arena0_program::abi::exports::LOCAL_MEMORY),
            Some(&(wasmparser::ExternalKind::Memory, 2))
        );
        crate::finalize::validate_finalized_shape(program.bytes(), engine.profile()).unwrap();
        let reparsed = engine.build_program(program.bytes()).unwrap();
        assert_eq!(reparsed.bytes(), program.bytes());
    }

    #[test]
    fn metadata_on_an_unfinalized_partial_artifact_is_rejected() {
        let engine = WasmtimeEngine::new().unwrap();
        let raw = dispatch_module("i64.const 0", Vec::new(), "");
        let definition = definition(Vec::new());
        let partial = Program::embed(&raw, &definition).unwrap();
        assert!(matches!(
            engine.build_program(partial.bytes()),
            Err(SandboxError::InvalidMetadata(message)) if message.contains("three-memory")
        ));
    }

    #[test]
    fn non_zero_canonical_state_tail_rejects_and_restores_both_payloads() {
        let engine = WasmtimeEngine::new().unwrap();
        let program = finalized_tail_program();
        let mut instance = engine
            .load(&program)
            .unwrap()
            .resident(
                arena0_program::SharedStateBytes::try_new(b"shared".to_vec()).unwrap(),
                arena0_program::LocalStateBytes::try_new(b"local".to_vec()).unwrap(),
            )
            .unwrap();
        let error = instance.dispatch(call()).unwrap_err();
        assert!(
            matches!(error, SandboxError::DispatchFailed(message) if message.contains("non-zero bytes"))
        );
        assert_eq!(instance.committed_payloads().0.as_bytes(), b"shared");
        assert_eq!(instance.committed_payloads().1.as_bytes(), b"local");
        let shared = canonical_state_view(
            &instance.store,
            instance.shared_memory,
            instance.shared_max_bytes,
            "shared",
        )
        .unwrap();
        assert_eq!(shared.payload, b"shared");
    }

    #[test]
    fn resident_dispatch_mutates_both_payloads_and_keeps_effects_host_owned() {
        let mut instance = resident(
            accepted_body(),
            vec![Capability::Messaging],
            r#"(import "arena0" "broadcast" (func $broadcast (param i32 i32)))"#,
        );
        let first = instance.dispatch(call()).unwrap();
        assert_eq!(first.status, arena0_program::CallStatus::Accepted);
        assert_eq!(first.shared.as_bytes(), b"sh");
        assert_eq!(first.local.as_bytes(), &[1]);
        assert_eq!(
            first.observations.effects,
            vec![Effect::Broadcast {
                data: b"effect".to_vec()
            }]
        );
        let first_hash = first.shared_hash;

        let second = instance.dispatch(call()).unwrap();
        assert_eq!(second.shared.as_bytes(), b"sh");
        // The mutable guest global is restored before each dispatch.
        assert_eq!(second.local.as_bytes(), &[1]);
        assert_eq!(second.shared_hash, first_hash);
        let canonical_shared = arena0_program::canonical_state_image(b"sh").unwrap();
        assert_eq!(
            first_hash,
            arena0_protocol::StateHash::of(&canonical_shared).0
        );
    }

    #[test]
    fn resident_freezes_prepared_work_memory_before_dispatch() {
        let mut instance = resident(growth_after_prepare_body(), Vec::new(), "");
        let result = instance.dispatch(call()).unwrap();
        assert_eq!(result.status, arena0_program::CallStatus::Accepted);
    }

    #[test]
    fn resident_enforces_metadata_shared_limit_on_recovery_and_dispatch() {
        let shared = arena0_program::SharedStateBytes::try_new(b"old".to_vec()).unwrap();
        let local = arena0_program::LocalStateBytes::try_new(b"keep".to_vec()).unwrap();
        let oversized = arena0_program::SharedStateBytes::try_new(vec![0; 65]).unwrap();
        let mut instance = resident(oversized_shared_body(), Vec::new(), "");

        assert!(matches!(
            instance.restore_payloads(oversized, local.clone()),
            Err(SandboxError::MemoryLimitExceeded(65))
        ));
        instance
            .restore_payloads(shared.clone(), local.clone())
            .unwrap();

        assert!(matches!(
            instance.dispatch(call()),
            Err(SandboxError::MemoryLimitExceeded(65))
        ));
        let (restored_shared, restored_local) = instance.commit_payloads().unwrap();
        assert_eq!(restored_shared, shared);
        assert_eq!(restored_local, local);
    }

    #[test]
    fn resident_dispatch_rolls_back_both_states_for_reject_and_trap() {
        let shared = arena0_program::SharedStateBytes::try_new(b"old".to_vec()).unwrap();
        let local = arena0_program::LocalStateBytes::try_new(b"keep".to_vec()).unwrap();

        let mut rejected = resident(rejected_body(), Vec::new(), "");
        rejected
            .restore_payloads(shared.clone(), local.clone())
            .unwrap();
        let result = rejected.dispatch(call()).unwrap();
        assert_eq!(result.status, arena0_program::CallStatus::Rejected);
        assert_eq!(result.shared.as_bytes(), b"old");
        assert_eq!(result.local.as_bytes(), b"keep");

        let mut trapped = resident("unreachable", Vec::new(), "");
        trapped
            .restore_payloads(shared.clone(), local.clone())
            .unwrap();
        assert!(trapped.dispatch(call()).is_err());
        assert_eq!(trapped.committed_payloads().0.as_bytes(), b"old");
        assert_eq!(trapped.committed_payloads().1.as_bytes(), b"keep");
    }

    #[test]
    fn rejected_dispatch_discards_state_and_all_provisional_observations() {
        let shared = arena0_program::SharedStateBytes::try_new(b"old".to_vec()).unwrap();
        let local = arena0_program::LocalStateBytes::try_new(b"keep".to_vec()).unwrap();
        let mut instance = resident(
            rejected_with_observations_body(),
            vec![Capability::Messaging],
            r#"
              (import "arena0" "broadcast" (func $broadcast (param i32 i32)))
              (import "arena0" "log" (func $log (param i32 i32 i32)))
              (import "arena0" "random" (func $random (param i32 i32)))
            "#,
        );
        instance
            .restore_payloads(shared.clone(), local.clone())
            .unwrap();

        let result = instance.dispatch(call()).unwrap();

        assert_eq!(result.status, arena0_program::CallStatus::Rejected);
        assert_eq!(result.shared, shared);
        assert_eq!(result.local, local);
        assert!(result.observations.effects.is_empty());
        assert!(result.observations.logs.is_empty());
        assert!(result.observations.random_draws.is_empty());
        assert_eq!(instance.committed_payloads(), (&shared, &local));
    }

    #[test]
    fn allocator_mutation_trap_rolls_back_both_state_memories() {
        let shared = arena0_program::SharedStateBytes::try_new(b"old".to_vec()).unwrap();
        let local = arena0_program::LocalStateBytes::try_new(b"keep".to_vec()).unwrap();
        let mut instance = allocator_failure_resident(
            r#"
              i32.const 100
              i32.const 1
              i32.store8 1
              i32.const 200
              i32.const 2
              i32.store8 2
              unreachable
            "#,
        );
        instance
            .restore_payloads(shared.clone(), local.clone())
            .unwrap();

        assert!(matches!(
            instance.dispatch(call()),
            Err(SandboxError::DispatchFailed(_))
        ));
        let (restored_shared, restored_local) = instance.commit_payloads().unwrap();
        assert_eq!(restored_shared, shared);
        assert_eq!(restored_local, local);
    }

    #[test]
    fn allocator_mutation_input_copy_failure_rolls_back_both_state_memories() {
        let shared = arena0_program::SharedStateBytes::try_new(b"old".to_vec()).unwrap();
        let local = arena0_program::LocalStateBytes::try_new(b"keep".to_vec()).unwrap();
        let mut instance = allocator_failure_resident(
            r#"
              i32.const 100
              i32.const 1
              i32.store8 1
              i32.const 200
              i32.const 2
              i32.store8 2
              i32.const 67108863
            "#,
        );
        instance
            .restore_payloads(shared.clone(), local.clone())
            .unwrap();

        assert!(matches!(
            instance.dispatch(call()),
            Err(SandboxError::MemoryLimitExceeded(_))
        ));
        let (restored_shared, restored_local) = instance.commit_payloads().unwrap();
        assert_eq!(restored_shared, shared);
        assert_eq!(restored_local, local);
    }

    #[test]
    fn resident_recovery_replaces_the_rollback_checkpoint() {
        let mut instance = resident("unreachable", Vec::new(), "");
        let shared = arena0_program::SharedStateBytes::try_new(b"recovered".to_vec()).unwrap();
        let local = arena0_program::LocalStateBytes::try_new(b"private".to_vec()).unwrap();
        instance
            .restore_payloads(shared.clone(), local.clone())
            .unwrap();
        assert_eq!(instance.committed_payloads(), (&shared, &local));
        instance.restore_committed().unwrap();
        assert_eq!(instance.committed_payloads().0.as_bytes(), b"recovered");
        assert_eq!(instance.committed_payloads().1.as_bytes(), b"private");
    }
}
