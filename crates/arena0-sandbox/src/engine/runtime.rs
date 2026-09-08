//! Fresh-instance execution operations for loaded programs.

use arena0_program::{
    CallStatus, InitInput, LocalInput, LocalOutput, OutcomeInput, OutcomeOutput, QueryInput,
    QueryOutput, SharedInput, SharedOutput, ViewInput, ViewOutput, WriterInput, WriterOutput, abi,
};
use arena0_protocol::Lifecycle;
use borsh::{BorshDeserialize, BorshSerialize};

use super::memory::Guest;
use super::{CallInstance, CallKind, InstanceConfig, instantiate_module, max_output};
use crate::call::{
    self, InitializeCall, LocalCall, OutcomeCall, QueryCall, SharedCall, ViewCall, WriterCall,
};
use crate::{
    CallObservations, GuestOutcomeResult, GuestProjectionResult, GuestWriterResult,
    InitializedState, LocalCallResult, SandboxError, SharedCallResult,
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

    /// Apply one shared/public event in a fresh guest instance.
    pub fn apply_shared(&self, call: SharedCall) -> Result<SharedCallResult, SandboxError> {
        let (shared, event, input_session, lifecycle) = match call {
            SharedCall::SessionStarted { shared, ensemble } => (
                shared,
                arena0_protocol::Event::SessionStarted { ensemble },
                None,
                Lifecycle::PreSession,
            ),
            SharedCall::Event {
                shared,
                session,
                event,
            } => (
                shared,
                event.into_protocol(),
                Some(session),
                Lifecycle::Active,
            ),
        };
        let event = call::serialize(&event)?;
        let input = call::shared_input(shared.clone(), event, input_session)?;
        self.validate_shared_state(&input.shared)?;
        let (output, observations) = self.invoke::<SharedInput, SharedOutput>(
            CallKind::Shared,
            lifecycle,
            None,
            abi::exports::SHARED,
            input,
        )?;
        if output.status == CallStatus::Rejected && !observations.is_empty_except_fuel() {
            return Err(SandboxError::DispatchFailed(
                "rejected shared call emitted observations".into(),
            ));
        }
        if output.status == CallStatus::Rejected && output.shared.as_bytes() != shared.as_bytes() {
            return Err(SandboxError::DispatchFailed(
                "rejected shared call changed shared state".into(),
            ));
        }
        self.validate_shared_state(&output.shared)?;
        Ok(SharedCallResult {
            status: output.status,
            shared: output.shared,
            observations,
        })
    }

    /// Apply one local/private event in a fresh guest instance.
    pub fn apply_local(&self, call: LocalCall) -> Result<LocalCallResult, SandboxError> {
        let (input, shared, random_replay) = call.into_input()?;
        self.validate_state(&input.shared, &input.local)?;
        let input_local = input.local.clone();
        let (output, observations) = self.invoke::<LocalInput, LocalOutput>(
            CallKind::Local,
            Lifecycle::Active,
            random_replay.as_ref().map(|replay| replay.as_slice()),
            abi::exports::LOCAL,
            input,
        )?;
        if output.status == CallStatus::Rejected {
            if output.local.as_bytes() != input_local.as_bytes()
                || !observations.is_empty_except_fuel()
            {
                return Err(SandboxError::DispatchFailed(
                    "rejected local call changed state or emitted observations".into(),
                ));
            }
            return Ok(LocalCallResult {
                status: output.status,
                local: input_local,
                observations,
            });
        }
        self.validate_state(&shared, &output.local)?;
        Ok(LocalCallResult {
            status: output.status,
            local: output.local,
            observations,
        })
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
            let returned = guest.call_pair_return(export, input_ptr, bytes_len);
            guest.dealloc(input_ptr, bytes_len)?;
            returned?
        };
        let fuel_used = self.profile.fuel.per_call.saturating_sub(
            instance
                .store
                .get_fuel()
                .unwrap_or(self.profile.fuel.per_call),
        );
        let output = self.decode_result::<O>(&mut instance, returned.0, returned.1)?;
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
mod fresh_runtime_tests {
    use std::sync::Arc;

    use super::*;
    use crate::call::{LocalEvent, SharedEvent};
    use crate::{LoadedProgram, Program, WasmtimeEngine};
    use arena0_program::{
        Capability, JsonBytes, JsonSchemaDocument, LocalStateBytes, ProgramDefinition,
        ProgramMetadata, ProgramSchema, QuerySchema, SharedStateBytes, StateSchema,
    };
    use arena0_protocol::{Committed, Ensemble, MessageId, PeerId, StateHash};

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
                name: "fresh-fixture".into(),
                version: "1".into(),
                description: "fresh guest fixture".into(),
                author: None,
                capabilities,
                display_name: "Fresh fixture".into(),
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
                queries: vec![QuerySchema {
                    name: "unit".into(),
                    label: "Unit".into(),
                    request: unit.clone(),
                    response: unit.clone(),
                }],
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

    fn bounded(bytes: &[u8]) -> Vec<u8> {
        let mut encoded = (bytes.len() as u32).to_le_bytes().to_vec();
        encoded.extend_from_slice(bytes);
        encoded
    }

    fn initialized(shared: &[u8], local: &[u8]) -> Vec<u8> {
        [bounded(shared), bounded(local)].concat()
    }

    fn shared_output(shared: &[u8]) -> Vec<u8> {
        [vec![0], bounded(shared)].concat()
    }

    fn local_output(local: &[u8]) -> Vec<u8> {
        [vec![0], bounded(local)].concat()
    }

    fn query_output(index: u32, json: &[u8]) -> Vec<u8> {
        [index.to_le_bytes().to_vec(), bounded(json)].concat()
    }

    fn projection_output(json: &[u8]) -> Vec<u8> {
        bounded(json)
    }

    fn outcome_output(borsh: &[u8], json: &[u8]) -> Vec<u8> {
        [bounded(borsh), bounded(json)].concat()
    }

    struct ModuleSpec<'a> {
        initialize: &'a [u8],
        shared: &'a [u8],
        shared_second: &'a [u8],
        local: &'a [u8],
        query: &'a [u8],
        view: &'a [u8],
        outcome: &'a [u8],
        shared_body: &'a str,
        local_body: &'a str,
        query_body: &'a str,
        view_body: &'a str,
        outcome_body: &'a str,
        capabilities: Vec<Capability>,
        imports: &'a str,
    }

    fn module(spec: ModuleSpec<'_>) -> Arc<LoadedProgram> {
        let initialize_data = wat_data(spec.initialize);
        let shared_data = wat_data(spec.shared);
        let shared_second_data = wat_data(spec.shared_second);
        let local_data = wat_data(spec.local);
        let query_data = wat_data(spec.query);
        let view_data = wat_data(spec.view);
        let outcome_data = wat_data(spec.outcome);
        let definition = definition(spec.capabilities);
        let metadata = definition.encode().unwrap();
        let metadata_data = wat_data(&metadata);
        let wat = format!(
            r#"
            (module
              {imports}
              (memory (export "memory") 2)
              (global (export "arena0_abi_version") i32 (i32.const 20))
              (global $shared_counter (mut i32) (i32.const 0))
              (data (i32.const 2048) "{initialize_data}")
              (data (i32.const 3072) "{shared_second_data}")
              (data (i32.const 4096) "{shared_data}")
              (data (i32.const 6144) "{local_data}")
              (data (i32.const 8192) "{query_data}")
              (data (i32.const 10240) "{view_data}")
              (data (i32.const 12288) "{outcome_data}")
              (data (i32.const 16384) "{metadata_data}")
              (func $pack (param $ptr i32) (param $len i32) (result i64)
                local.get $ptr
                i64.extend_i32_s
                i64.const 32
                i64.shl
                local.get $len
                i64.extend_i32_s
                i64.const 4294967295
                i64.and
                i64.or)
              ;; Hostile fixture guard: 0xa5 is reserved for a local-state or
              ;; peer-identity sentinel. Shared and projection exports trap if
              ;; that byte is present in their input envelope.
              (func $assert_no_private (param $ptr i32) (param $len i32)
                (local $offset i32)
                (block $done
                  (loop $scan
                    local.get $offset
                    local.get $len
                    i32.ge_u
                    br_if $done
                    local.get $ptr
                    local.get $offset
                    i32.add
                    i32.load8_u
                    i32.const 165
                    i32.eq
                    if
                      unreachable
                    end
                    local.get $offset
                    i32.const 1
                    i32.add
                    local.set $offset
                    br $scan)))
              (func $initialize (result i64)
                i32.const 2048
                i32.const {initialize_len}
                call $pack)
              (func $shared_default (result i64)
                i32.const 4096
                i32.const {shared_len}
                call $pack)
              (func $shared_second (result i64)
                i32.const 3072
                i32.const {shared_second_len}
                call $pack)
              (func $local_default (result i64)
                i32.const 6144
                i32.const {local_len}
                call $pack)
              (func $query_default (result i64)
                i32.const 8192
                i32.const {query_len}
                call $pack)
              (func $view_default (result i64)
                i32.const 10240
                i32.const {view_len}
                call $pack)
              (func $outcome_default (result i64)
                i32.const 12288
                i32.const {outcome_len}
                call $pack)
              (func $metadata (result i64)
                i32.const 16384
                i32.const {metadata_len}
                call $pack)
              (func (export "arena0_alloc") (param i32) (result i32) i32.const 1024)
              (func (export "arena0_dealloc") (param i32 i32))
              (func (export "arena0_initialize") (param $input_ptr i32) (param $input_len i32) (result i64)
                local.get $input_ptr
                local.get $input_len
                call $assert_no_private
                call $initialize)
              (func (export "arena0_shared") (param $input_ptr i32) (param $input_len i32) (result i64)
                local.get $input_ptr
                local.get $input_len
                call $assert_no_private
                {shared_body})
              (func (export "arena0_local") (param i32 i32) (result i64)
                {local_body})
              (func (export "arena0_writer") (param $input_ptr i32) (param $input_len i32) (result i64)
                local.get $input_ptr
                local.get $input_len
                call $assert_no_private
                i32.const 2112
                i32.const 1
                call $pack)
              (func (export "arena0_query") (param $input_ptr i32) (param $input_len i32) (result i64)
                local.get $input_ptr
                local.get $input_len
                call $assert_no_private
                {query_body})
              (func (export "arena0_view") (param $input_ptr i32) (param $input_len i32) (result i64)
                local.get $input_ptr
                local.get $input_len
                call $assert_no_private
                {view_body})
              (func (export "arena0_outcome") (param $input_ptr i32) (param $input_len i32) (result i64)
                local.get $input_ptr
                local.get $input_len
                call $assert_no_private
                {outcome_body})
              (func (export "arena0_metadata") (result i64)
                call $metadata))
            "#,
            initialize_data = initialize_data,
            shared_data = shared_data,
            shared_second_data = shared_second_data,
            local_data = local_data,
            query_data = query_data,
            view_data = view_data,
            outcome_data = outcome_data,
            metadata_data = metadata_data,
            initialize_len = spec.initialize.len(),
            shared_len = spec.shared.len(),
            shared_second_len = spec.shared_second.len(),
            local_len = spec.local.len(),
            query_len = spec.query.len(),
            view_len = spec.view.len(),
            outcome_len = spec.outcome.len(),
            metadata_len = metadata.len(),
            shared_body = spec.shared_body,
            local_body = spec.local_body,
            query_body = spec.query_body,
            view_body = spec.view_body,
            outcome_body = spec.outcome_body,
            imports = spec.imports,
        );
        let binary = wat::parse_str(wat).unwrap();
        let program = Program::embed(&binary, &definition).unwrap();
        WasmtimeEngine::new().unwrap().load(&program).unwrap()
    }

    fn peers() -> (PeerId, Ensemble<Committed>) {
        let local = PeerId([1; 32]);
        let remote = PeerId([2; 32]);
        let session = Ensemble::from_peers(vec![local, remote]).unwrap();
        (local, session)
    }

    fn empty_state() -> (SharedStateBytes, LocalStateBytes) {
        (
            SharedStateBytes::try_new(Vec::new()).unwrap(),
            LocalStateBytes::try_new(Vec::new()).unwrap(),
        )
    }

    fn message_event() -> SharedEvent {
        SharedEvent::MessageReceived {
            message_id: MessageId([3; 32]),
            from: PeerId([2; 32]),
            position: 0,
            pre_state: StateHash([0; 32]),
            msg: Vec::new(),
        }
    }

    fn base_spec<'a>(
        initialize: &'a [u8],
        shared: &'a [u8],
        local: &'a [u8],
        query: &'a [u8],
        view: &'a [u8],
        outcome: &'a [u8],
    ) -> ModuleSpec<'a> {
        ModuleSpec {
            initialize,
            shared,
            shared_second: shared,
            local,
            query,
            view,
            outcome,
            shared_body: "call $shared_default",
            local_body: "call $local_default",
            query_body: "call $query_default",
            view_body: "call $view_default",
            outcome_body: "call $outcome_default",
            capabilities: Vec::new(),
            imports: "",
        }
    }

    #[test]
    fn fresh_shared_calls_do_not_carry_or_replace_local_state() {
        let init = initialized(&[], &[9]);
        let first_output = shared_output(&[1]);
        let second_output = shared_output(&[2]);
        let local_output = local_output(&[8]);
        let query = query_output(0, b"null");
        let view = projection_output(b"null");
        let outcome = outcome_output(&[7], b"null");
        let program = module(ModuleSpec {
            initialize: &init,
            shared: &first_output,
            shared_second: &second_output,
            local: &local_output,
            query: &query,
            view: &view,
            outcome: &outcome,
            shared_body: r#"
                global.get $shared_counter
                i32.const 1
                i32.add
                global.set $shared_counter
                global.get $shared_counter
                i32.const 1
                i32.eq
                if (result i64)
                  call $shared_default
                else
                  call $shared_second
                end
            "#,
            local_body: "call $local_default",
            query_body: "call $query_default",
            view_body: "call $view_default",
            outcome_body: "call $outcome_default",
            capabilities: Vec::new(),
            imports: "",
        });
        let (peer, session) = peers();
        let (shared, local) = empty_state();
        let initialized = program
            .initialize(InitializeCall::new(
                JsonBytes::try_new(b"null".to_vec()).unwrap(),
            ))
            .unwrap();
        assert_eq!(initialized.local.as_bytes(), &[9]);
        let first = program
            .apply_shared(SharedCall::session_started(shared, session.clone()))
            .unwrap();
        assert_eq!(first.shared.as_bytes(), &[1]);
        let local_result = program
            .apply_local(LocalCall::new(
                peer,
                first.shared.clone(),
                local,
                session.clone(),
                LocalEvent::React,
            ))
            .unwrap();
        assert_eq!(local_result.local.as_bytes(), &[8]);
        let second = program
            .apply_shared(SharedCall::new(first.shared, session, message_event()))
            .unwrap();
        // A fresh instance starts its guest globals at their declared initial
        // values for every call; state crosses the boundary only as bytes.
        assert_eq!(second.shared.as_bytes(), &[1]);
        // The local value is owned by the caller and remains the local result;
        // SharedOutput has no field through which the guest could replace it.
        assert_eq!(local_result.local.as_bytes(), &[8]);
    }

    #[test]
    fn fresh_local_calls_cannot_replace_shared_state() {
        let init = initialized(&[], &[]);
        let shared_output = shared_output(&[]);
        let local_output = local_output(&[4]);
        let query = query_output(0, b"null");
        let view = projection_output(b"null");
        let outcome = outcome_output(&[7], b"null");
        let program = module(base_spec(
            &init,
            &shared_output,
            &local_output,
            &query,
            &view,
            &outcome,
        ));
        let (peer, session) = peers();
        let shared = SharedStateBytes::try_new(vec![5]).unwrap();
        let result = program
            .apply_local(LocalCall::new(
                peer,
                shared.clone(),
                LocalStateBytes::try_new(Vec::new()).unwrap(),
                session,
                LocalEvent::React,
            ))
            .unwrap();
        assert_eq!(result.local.as_bytes(), &[4]);
        // The shared state remains owned by the caller; LocalOutput carries no
        // shared field through which the guest could replace it.
        assert_eq!(shared.as_bytes(), &[5]);
    }

    #[test]
    fn read_only_exports_return_only_their_projection_values() {
        let init = initialized(&[], &[]);
        let shared = shared_output(&[]);
        let local = local_output(&[]);
        let query = query_output(0, b"null");
        let view = projection_output(b"null");
        let outcome = outcome_output(&[7, 8], b"null");
        let program = module(base_spec(&init, &shared, &local, &query, &view, &outcome));
        let (_, session) = peers();
        let state = SharedStateBytes::try_new(vec![6]).unwrap();
        let writer = program
            .writer(WriterCall::new(state.clone(), session.clone()))
            .unwrap();
        assert_eq!(writer.writer, None);
        let query_result = program
            .query(QueryCall::new(
                state.clone(),
                session.clone(),
                JsonBytes::try_new(b"null".to_vec()).unwrap(),
                0,
            ))
            .unwrap();
        assert_eq!(query_result.output.as_bytes(), b"null");
        let view_result = program
            .view(ViewCall::new(
                state.clone(),
                session.clone(),
                JsonBytes::try_new(b"null".to_vec()).unwrap(),
            ))
            .unwrap();
        assert_eq!(view_result.output.as_bytes(), b"null");
        let outcome_result = program.outcome(OutcomeCall::new(state, session)).unwrap();
        assert_eq!(outcome_result.borsh.as_bytes(), &[7, 8]);
        assert_eq!(outcome_result.json.as_bytes(), b"null");
    }

    #[test]
    fn malformed_and_oversized_guest_envelopes_are_rejected() {
        let init = initialized(&[], &[]);
        let malformed = vec![0xff];
        let valid_shared = shared_output(&[]);
        let valid_local = local_output(&[]);
        let valid_query = query_output(0, b"null");
        let valid_view = projection_output(b"null");
        let valid_outcome = outcome_output(&[], b"null");
        let program = module(base_spec(
            &init,
            &valid_shared,
            &valid_local,
            &malformed,
            &valid_view,
            &valid_outcome,
        ));
        let (_, session) = peers();
        let error = program
            .query(QueryCall::new(
                SharedStateBytes::try_new(Vec::new()).unwrap(),
                session.clone(),
                JsonBytes::try_new(b"null".to_vec()).unwrap(),
                0,
            ))
            .unwrap_err();
        assert!(matches!(error, SandboxError::DeserializationFailed(_)));

        let oversized = [0u8; 1];
        let program = module(ModuleSpec {
            initialize: &init,
            shared: &valid_shared,
            shared_second: &valid_shared,
            local: &valid_local,
            query: &valid_query,
            view: &valid_view,
            outcome: &valid_outcome,
            shared_body: "i32.const 2048 i32.const 16777217 call $pack",
            local_body: "call $local_default",
            query_body: "call $query_default",
            view_body: "call $view_default",
            outcome_body: "call $outcome_default",
            capabilities: Vec::new(),
            imports: "",
        });
        let (peer, session) = peers();
        let error = program
            .apply_shared(SharedCall::new(
                SharedStateBytes::try_new(oversized.to_vec()).unwrap(),
                session,
                message_event(),
            ))
            .unwrap_err();
        assert!(matches!(error, SandboxError::OutputLimitExceeded { .. }));
        let _ = peer;
    }

    #[test]
    fn read_only_effect_attempt_traps_before_it_can_escape() {
        let init = initialized(&[], &[]);
        let shared = shared_output(&[]);
        let local = local_output(&[]);
        let query = query_output(0, b"null");
        let view = projection_output(b"null");
        let outcome = outcome_output(&[], b"null");
        let log_import = r#"
              (import "arena0" "log" (func $log (param i32 i32 i32)))
        "#;
        let program = module(ModuleSpec {
            initialize: &init,
            shared: &shared,
            shared_second: &shared,
            local: &local,
            query: &query,
            view: &view,
            outcome: &outcome,
            shared_body: "call $shared_default",
            local_body: "call $local_default",
            query_body: "i32.const 0 i32.const 0 i32.const 0 call $log call $query_default",
            view_body: "call $view_default",
            outcome_body: "call $outcome_default",
            capabilities: Vec::new(),
            imports: log_import,
        });
        let (_, session) = peers();
        let error = program
            .query(QueryCall::new(
                SharedStateBytes::try_new(Vec::new()).unwrap(),
                session,
                JsonBytes::try_new(b"null".to_vec()).unwrap(),
                0,
            ))
            .unwrap_err();
        assert!(matches!(error, SandboxError::DispatchFailed(_)));
    }

    #[test]
    fn guest_traps_are_reported_as_dispatch_failures() {
        let init = initialized(&[], &[]);
        let shared = shared_output(&[]);
        let local = local_output(&[]);
        let query = query_output(0, b"null");
        let view = projection_output(b"null");
        let outcome = outcome_output(&[], b"null");
        let program = module(ModuleSpec {
            initialize: &init,
            shared: &shared,
            shared_second: &shared,
            local: &local,
            query: &query,
            view: &view,
            outcome: &outcome,
            shared_body: "unreachable",
            local_body: "call $local_default",
            query_body: "call $query_default",
            view_body: "call $view_default",
            outcome_body: "call $outcome_default",
            capabilities: Vec::new(),
            imports: "",
        });
        let (_, session) = peers();
        let error = program
            .apply_shared(SharedCall::new(
                SharedStateBytes::try_new(Vec::new()).unwrap(),
                session,
                message_event(),
            ))
            .unwrap_err();
        assert!(matches!(error, SandboxError::DispatchFailed(_)));
    }
}
