//! Wasm module validation: required exports, import capability checks, and ABI
//! version verification.

use std::collections::HashSet;

use arena0_program::abi;
use arena0_program::{JsonSchemaDocument, ProgramDefinition, ProgramMetadata};
use wasmtime::{ExternType, FuncType, Module, ValType};

use crate::SandboxError;

/// The single program ABI version this sandbox links against.
const EXPECTED_ABI_VERSION: u32 = abi::ABI_VERSION;

const REQUIRED_FUNC_EXPORTS: &[(&str, &[AbiType], Returns)] = &[
    (
        abi::exports::ALLOC,
        &[AbiType::I32],
        Returns::One(AbiType::I32),
    ),
    (
        abi::exports::DEALLOC,
        &[AbiType::I32, AbiType::I32],
        Returns::None,
    ),
    (
        abi::exports::INITIALIZE,
        &[AbiType::I32, AbiType::I32],
        Returns::Packed,
    ),
    (
        abi::exports::SHARED,
        &[AbiType::I32, AbiType::I32],
        Returns::Packed,
    ),
    (
        abi::exports::LOCAL,
        &[AbiType::I32, AbiType::I32],
        Returns::Packed,
    ),
    (
        abi::exports::WRITER,
        &[AbiType::I32, AbiType::I32],
        Returns::Packed,
    ),
    (
        abi::exports::OUTCOME,
        &[AbiType::I32, AbiType::I32],
        Returns::Packed,
    ),
    (
        abi::exports::QUERY,
        &[AbiType::I32, AbiType::I32],
        Returns::Packed,
    ),
    (
        abi::exports::VIEW,
        &[AbiType::I32, AbiType::I32],
        Returns::Packed,
    ),
    (abi::exports::METADATA, &[], Returns::Packed),
];

const REQUIRED_GLOBAL_EXPORTS: &[&str] = &["arena0_abi_version"];

/// Globals at or above this value are assumed to be pointers into linear
/// memory (the Rust wasm32 pattern for `#[no_mangle] static` items) and
/// are dereferenced rather than read as direct values.
const GLOBAL_POINTER_THRESHOLD: u32 = 65_536;

/// Validate every embedded JSON Schema document before the sandbox accepts
/// metadata. Schemas are introspection-only; this is admission hygiene, not a
/// serialization contract.
pub(crate) fn validate_program_definition(
    definition: &ProgramDefinition,
) -> Result<(), SandboxError> {
    let participants = definition.metadata.participants;
    if let Err(error) = participants.validate() {
        return Err(SandboxError::InvalidMetadata(format!(
            "invalid participant count {participants}: {error}"
        )));
    }
    let (min, max) = participants.bounds();
    if usize::from(min) < 2 || usize::from(max) > arena0_protocol::MAX_PARTICIPANTS {
        return Err(SandboxError::InvalidMetadata(format!(
            "participant count must be between 2 and {}, got {participants}",
            arena0_protocol::MAX_PARTICIPANTS
        )));
    }

    let schema = &definition.schema;
    validate_unique_names(
        "callout",
        schema.callouts.iter().map(|callout| callout.name.as_str()),
    )?;
    validate_unique_names(
        "query",
        schema.queries.iter().map(|query| query.name.as_str()),
    )?;
    validate_json_schema("state", &schema.state.schema)?;
    for callout in &schema.callouts {
        validate_json_schema(&format!("callout {} input", callout.name), &callout.input)?;
        validate_json_schema(&format!("callout {} output", callout.name), &callout.output)?;
    }
    validate_json_schema("params", &schema.params)?;
    for query in &schema.queries {
        validate_json_schema(&format!("query {} request", query.name), &query.request)?;
        validate_json_schema(&format!("query {} response", query.name), &query.response)?;
    }
    validate_json_schema("outcome", &schema.outcome)
}

fn validate_unique_names<'a>(
    kind: &str,
    names: impl IntoIterator<Item = &'a str>,
) -> Result<(), SandboxError> {
    let mut seen = HashSet::new();
    for name in names {
        if name.is_empty() {
            return Err(SandboxError::InvalidMetadata(format!(
                "program {kind} name must not be empty"
            )));
        }
        if !seen.insert(name) {
            return Err(SandboxError::InvalidMetadata(format!(
                "duplicate program {kind} name: {name}"
            )));
        }
    }
    Ok(())
}

fn validate_json_schema(label: &str, document: &JsonSchemaDocument) -> Result<(), SandboxError> {
    document.validate_local_references().map_err(|error| {
        SandboxError::InvalidMetadata(format!("{label} JSON Schema is invalid: {error}"))
    })?;
    jsonschema::draft202012::meta::validate(document.as_value()).map_err(|error| {
        SandboxError::InvalidMetadata(format!("{label} JSON Schema is invalid: {error}"))
    })
}

/// Verify the module exports all required arena0 function and global symbols.
pub(crate) fn validate_exports(module: &Module) -> Result<(), SandboxError> {
    for &(name, params, returns) in REQUIRED_FUNC_EXPORTS {
        let export = module
            .exports()
            .find(|export| export.name() == name)
            .ok_or_else(|| SandboxError::MissingExport(name.to_string()))?;
        let ExternType::Func(function) = export.ty() else {
            return Err(SandboxError::MissingExport(name.to_string()));
        };
        validate_func_signature(name, &function, params, returns)?;
    }

    for &name in REQUIRED_GLOBAL_EXPORTS {
        let found = module
            .exports()
            .any(|e| e.name() == name && e.ty().global().is_some());
        if !found {
            return Err(SandboxError::MissingExport(name.to_string()));
        }
    }

    Ok(())
}

#[derive(Clone, Copy)]
enum AbiType {
    I32,
}

impl AbiType {
    fn matches(self, actual: &ValType) -> bool {
        match self {
            Self::I32 => actual.is_i32(),
        }
    }
}

#[derive(Clone, Copy)]
enum Returns {
    None,
    One(AbiType),
    /// The SDK's canonical packed i64 pointer/length return.
    Packed,
}

fn validate_func_signature(
    name: &str,
    function: &FuncType,
    expected_params: &[AbiType],
    expected_returns: Returns,
) -> Result<(), SandboxError> {
    let params: Vec<_> = function.params().collect();
    let results: Vec<_> = function.results().collect();
    let returns_match = match expected_returns {
        Returns::None => results.is_empty(),
        Returns::One(result) => results.len() == 1 && result.matches(&results[0]),
        Returns::Packed => results.len() == 1 && results[0].is_i64(),
    };
    let params_match = params.len() == expected_params.len()
        && params
            .iter()
            .zip(expected_params)
            .all(|(actual, expected)| expected.matches(actual));
    if params_match && returns_match {
        return Ok(());
    }
    let expected = match expected_returns {
        Returns::None => "declared parameters -> ()",
        Returns::One(AbiType::I32) => "declared parameters -> i32",
        Returns::Packed => "declared parameters -> i64",
    };
    Err(SandboxError::InvalidExportSignature {
        name: name.to_string(),
        expected,
        actual: format!("{params:?} -> {results:?}"),
    })
}

/// Reject modules that import host functions beyond their declared capabilities.
pub(crate) fn validate_imports(
    module: &Module,
    metadata: &ProgramMetadata,
) -> Result<(), SandboxError> {
    let mut allowed: HashSet<&str> = abi::always_available_imports().iter().copied().collect();
    for cap in &metadata.capabilities {
        allowed.extend(cap.imports());
    }

    for import in module.imports() {
        if import.module() != abi::HOST_MODULE {
            return Err(SandboxError::InvalidImport {
                module: import.module().to_string(),
                name: import.name().to_string(),
            });
        }
        if !allowed.contains(import.name()) {
            return Err(SandboxError::CapabilityViolation {
                action: import.name().to_string(),
                capability: "undeclared".to_string(),
            });
        }
    }
    Ok(())
}

/// Read an i32 global, dereferencing through linear memory when the value
/// exceeds `GLOBAL_POINTER_THRESHOLD`.
fn read_global_i32(
    store: &mut wasmtime::Store<super::engine::HostState>,
    instance: &wasmtime::Instance,
    name: &str,
) -> Result<u32, SandboxError> {
    let global = instance
        .get_global(&mut *store, name)
        .ok_or_else(|| SandboxError::MissingExport(name.into()))?;

    let raw = match global.get(&mut *store) {
        wasmtime::Val::I32(v) => v as u32,
        _ => {
            return Err(SandboxError::dispatch_failed(format!(
                "{name} is not an i32"
            )));
        }
    };

    if raw < GLOBAL_POINTER_THRESHOLD {
        return Ok(raw);
    }

    // Rust wasm32 pattern: the global holds a pointer into linear memory.
    let memory = instance
        .get_memory(&mut *store, "memory")
        .ok_or_else(|| SandboxError::dispatch_failed("no 'memory' export"))?;
    let data = memory.data(&*store);
    let addr = raw as usize;
    let end = addr.checked_add(4).ok_or_else(|| {
        SandboxError::dispatch_failed(format!("{name}: pointer arithmetic overflow"))
    })?;
    if end > data.len() {
        return Err(SandboxError::dispatch_failed(format!(
            "{name}: pointer {raw:#x} out of memory bounds"
        )));
    }
    let value = u32::from_le_bytes([data[addr], data[addr + 1], data[addr + 2], data[addr + 3]]);
    Ok(value)
}

pub(crate) fn check_abi_version(
    store: &mut wasmtime::Store<super::engine::HostState>,
    instance: &wasmtime::Instance,
) -> Result<(), SandboxError> {
    let version = read_global_i32(store, instance, "arena0_abi_version")?;

    if version != EXPECTED_ABI_VERSION {
        return Err(SandboxError::InvalidAbiVersion {
            expected: EXPECTED_ABI_VERSION,
            actual: version,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::{Engine, Instance, Module, Store};

    /// A stub module exporting every required arena0 symbol, optionally omitting
    /// one export so each required-export check stays isolated.
    fn stub_module(
        with_writer: bool,
        with_outcome: bool,
        with_view: bool,
        valid_alloc: bool,
    ) -> Module {
        let alloc = if valid_alloc {
            r#"(func (export "arena0_alloc") (param i32) (result i32) i32.const 0)"#
        } else {
            r#"(func (export "arena0_alloc") (result i32) i32.const 0)"#
        };
        let outcome = if with_outcome {
            r#"(func (export "arena0_outcome") (param i32 i32) (result i64) i64.const 0)"#
        } else {
            ""
        };
        let writer = if with_writer {
            r#"(func (export "arena0_writer") (param i32 i32) (result i64) i64.const 0)"#
        } else {
            ""
        };
        let view = if with_view {
            r#"(func (export "arena0_view") (param i32 i32) (result i64) i64.const 0)"#
        } else {
            ""
        };
        let wat = format!(
            r#"
            (module
              (memory (export "memory") 1)
              (global (export "arena0_abi_version") i32 (i32.const 20))
              {alloc}
              (func (export "arena0_dealloc") (param i32 i32))
              (func (export "arena0_initialize") (param i32 i32) (result i64) i64.const 0)
              (func (export "arena0_shared") (param i32 i32) (result i64) i64.const 0)
              (func (export "arena0_local") (param i32 i32) (result i64) i64.const 0)
              {writer}
              {outcome}
              (func (export "arena0_query") (param i32 i32) (result i64) i64.const 0)
              {view}
              (func (export "arena0_metadata") (result i64) i64.const 0))
            "#
        );
        Module::new(&Engine::default(), wat).unwrap()
    }

    #[test]
    fn validate_exports_accepts_full_required_set() {
        assert!(validate_exports(&stub_module(true, true, true, true)).is_ok());
    }

    #[test]
    fn validate_exports_requires_arena0_writer() {
        let err = validate_exports(&stub_module(false, true, true, true)).unwrap_err();
        assert!(matches!(err, SandboxError::MissingExport(name) if name == "arena0_writer"));
    }

    #[test]
    fn validate_exports_requires_arena0_outcome() {
        let err = validate_exports(&stub_module(true, false, true, true)).unwrap_err();
        assert!(matches!(err, SandboxError::MissingExport(name) if name == "arena0_outcome"));
    }

    #[test]
    fn validate_exports_requires_arena0_view() {
        let err = validate_exports(&stub_module(true, true, false, true)).unwrap_err();
        assert!(matches!(err, SandboxError::MissingExport(name) if name == "arena0_view"));
    }

    #[test]
    fn validate_exports_rejects_wrong_signature() {
        let err = validate_exports(&stub_module(true, true, true, false)).unwrap_err();
        assert!(matches!(
            err,
            SandboxError::InvalidExportSignature { name, .. } if name == "arena0_alloc"
        ));
    }

    #[test]
    fn abi_16_is_rejected_with_expected_version() {
        let module = Module::new(
            &Engine::default(),
            r#"(module (global (export "arena0_abi_version") i32 (i32.const 16)))"#,
        )
        .unwrap();
        let engine = module.engine().clone();
        let mut store = Store::new(
            &engine,
            crate::engine::HostState::new(
                arena0_program::ExecutionProfile::current(),
                crate::engine::CallKind::Metadata,
                arena0_protocol::Lifecycle::PreSession,
                None,
                Vec::new(),
            ),
        );
        let instance = Instance::new(&mut store, &module, &[]).unwrap();

        let err = check_abi_version(&mut store, &instance).unwrap_err();
        assert!(matches!(
            err,
            SandboxError::InvalidAbiVersion {
                expected: EXPECTED_ABI_VERSION,
                actual: 16
            }
        ));
    }
}
