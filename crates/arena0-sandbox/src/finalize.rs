//! ABI-22 Wasm finalization and structural validation.
//!
//! Rust's wasm target emits one ordinary linear memory. The arena0 artifact
//! boundary turns that memory into bounded work memory and appends the two state
//! memories. A resident runtime freezes the observed work size after allocator
//! preparation. This is deliberately a structural rewrite: it does not alter
//! guest code, imports, data, or function indices.

use std::collections::{BTreeMap, BTreeSet};

use arena0_program::{
    ExecutionProfile, MAX_WASM_MEMORY_BYTES, MAX_WASM_STATE_MEMORY_BYTES, MAX_WASM_TABLE_ELEMENTS,
    abi,
};
use wasm_encoder::reencode::Reencode;
use wasm_encoder::{MemoryType, TableType};
use wasmparser::{DataKind, ElementKind, ExternalKind, Operator, Parser, Payload, TypeRef};

use crate::SandboxError;

/// Reserved export prefix for mutable globals captured by a resident instance.
pub(crate) const MUTABLE_GLOBAL_EXPORT_PREFIX: &str = "arena0_global_";

#[derive(Debug, Default)]
pub(crate) struct ModuleShape {
    pub(crate) defined_memories: Vec<(u64, Option<u64>)>,
    pub(crate) imported_memories: u32,
    pub(crate) imported_globals: u32,
    pub(crate) imported_tables: u32,
    pub(crate) defined_globals: Vec<bool>,
    pub(crate) defined_tables: Vec<(u64, Option<u64>)>,
    pub(crate) exports: BTreeMap<String, (ExternalKind, u32)>,
    pub(crate) has_memory_section: bool,
    pub(crate) has_export_section: bool,
}

impl ModuleShape {
    fn mutable_global_indices(&self) -> impl Iterator<Item = u32> + '_ {
        self.defined_globals
            .iter()
            .enumerate()
            .filter_map(|(index, mutable)| mutable.then_some(index as u32))
    }

    fn exports_function(&self, name: &str) -> bool {
        self.exports
            .get(name)
            .is_some_and(|(kind, _)| *kind == ExternalKind::Func)
    }
}

/// Inspect a core module and reject state that cannot be reset at a resident
/// call boundary. Wasmtime performs ordinary validation; these checks enforce
/// arena0's stronger ownership contract.
pub(crate) fn inspect(binary: &[u8]) -> Result<ModuleShape, SandboxError> {
    let mut shape = ModuleShape::default();
    for payload in Parser::new(0).parse_all(binary) {
        let payload =
            payload.map_err(|error| SandboxError::compilation_failed(error.to_string()))?;
        match payload {
            Payload::ImportSection(section) => {
                for import in section.into_imports() {
                    let import = import
                        .map_err(|error| SandboxError::compilation_failed(error.to_string()))?;
                    match import.ty {
                        TypeRef::Memory(_) => shape.imported_memories += 1,
                        TypeRef::Global(_) => shape.imported_globals += 1,
                        TypeRef::Table(_) => shape.imported_tables += 1,
                        _ => {}
                    }
                }
            }
            Payload::MemorySection(section) => {
                shape.has_memory_section = true;
                for memory in section {
                    let memory = memory
                        .map_err(|error| SandboxError::compilation_failed(error.to_string()))?;
                    shape
                        .defined_memories
                        .push((memory.initial, memory.maximum));
                }
            }
            Payload::GlobalSection(section) => {
                for global in section {
                    let global = global
                        .map_err(|error| SandboxError::compilation_failed(error.to_string()))?;
                    shape.defined_globals.push(global.ty.mutable);
                }
            }
            Payload::TableSection(section) => {
                for table in section {
                    let table = table
                        .map_err(|error| SandboxError::compilation_failed(error.to_string()))?;
                    shape
                        .defined_tables
                        .push((table.ty.initial, table.ty.maximum));
                }
            }
            Payload::ExportSection(section) => {
                shape.has_export_section = true;
                for export in section {
                    let export = export
                        .map_err(|error| SandboxError::compilation_failed(error.to_string()))?;
                    if shape
                        .exports
                        .insert(export.name.to_owned(), (export.kind, export.index))
                        .is_some()
                    {
                        return Err(SandboxError::InvalidMetadata(format!(
                            "duplicate Wasm export name: {}",
                            export.name
                        )));
                    }
                }
            }
            Payload::DataSection(section) => {
                for data in section {
                    let data =
                        data.map_err(|error| SandboxError::compilation_failed(error.to_string()))?;
                    if !matches!(
                        data.kind,
                        DataKind::Active {
                            memory_index: 0,
                            ..
                        }
                    ) {
                        return Err(SandboxError::InvalidMetadata(
                            "ABI-22 modules may only use active data segments in work memory"
                                .into(),
                        ));
                    }
                }
            }
            Payload::ElementSection(section) => {
                for element in section {
                    let element = element
                        .map_err(|error| SandboxError::compilation_failed(error.to_string()))?;
                    if !matches!(element.kind, ElementKind::Active { .. }) {
                        return Err(SandboxError::InvalidMetadata(
                            "ABI-22 modules may not contain passive or declared element segments"
                                .into(),
                        ));
                    }
                }
            }
            Payload::CodeSectionEntry(body) => {
                let mut operators = body
                    .get_operators_reader()
                    .map_err(|error| SandboxError::compilation_failed(error.to_string()))?;
                while !operators.eof() {
                    reject_operator(
                        operators
                            .read()
                            .map_err(|error| SandboxError::compilation_failed(error.to_string()))?,
                    )?;
                }
            }
            _ => {}
        }
    }
    Ok(shape)
}

fn reject_operator(operator: Operator<'_>) -> Result<(), SandboxError> {
    let reason = match operator {
        Operator::DataDrop { .. } => Some("data.drop"),
        Operator::ElemDrop { .. } => Some("elem.drop"),
        Operator::TableSet { .. }
        | Operator::TableGrow { .. }
        | Operator::TableFill { .. }
        | Operator::TableCopy { .. }
        | Operator::TableInit { .. } => Some("table mutation"),
        Operator::TableAtomicSet { .. }
        | Operator::TableAtomicRmwXchg { .. }
        | Operator::TableAtomicRmwCmpxchg { .. } => Some("atomic table mutation"),
        _ => None,
    };
    if let Some(reason) = reason {
        return Err(SandboxError::InvalidMetadata(format!(
            "ABI-22 modules may not use {reason}"
        )));
    }
    Ok(())
}

fn memory_bytes(pages: u64) -> Option<u64> {
    pages.checked_mul(65_536)
}

fn work_memory_fits_profile(work_pages: u64, profile: &ExecutionProfile) -> bool {
    work_pages > 0
        && memory_bytes(work_pages).is_some_and(|bytes| {
            bytes <= profile.limits.max_memory_bytes && bytes <= MAX_WASM_MEMORY_BYTES as u64
        })
}

fn profile_work_max_pages(profile: &ExecutionProfile) -> u64 {
    let max_bytes = profile
        .limits
        .max_memory_bytes
        .min(MAX_WASM_MEMORY_BYTES as u64);
    max_bytes / 65_536
}

fn prepared_work_capacity_fits_profile(work_pages: u64, profile: &ExecutionProfile) -> bool {
    memory_bytes(work_pages)
        .is_some_and(|bytes| bytes >= profile.limits.min_prepared_work_memory_bytes)
}

fn aggregate_memory_fits_profile(
    work_pages: u64,
    state_pages: u64,
    profile: &ExecutionProfile,
) -> bool {
    let Some(work_bytes) = memory_bytes(work_pages) else {
        return false;
    };
    let Some(state_bytes) = memory_bytes(state_pages) else {
        return false;
    };
    let Some(state_bytes) = state_bytes.checked_mul(2) else {
        return false;
    };
    let Some(total) = work_bytes.checked_add(state_bytes) else {
        return false;
    };
    total <= profile.limits.max_total_memory_bytes
}

fn mutable_global_name(index: u32) -> String {
    format!("{MUTABLE_GLOBAL_EXPORT_PREFIX}{index}")
}

/// Rewrite a raw ABI-22 module into the completed three-memory artifact with
/// a bounded work-memory maximum.
pub(crate) fn finalize(binary: &[u8], profile: &ExecutionProfile) -> Result<Vec<u8>, SandboxError> {
    let shape = inspect(binary)?;
    if shape.imported_memories != 0 {
        return Err(SandboxError::InvalidMetadata(
            "ABI-22 modules may not import linear memories".into(),
        ));
    }
    if shape.imported_globals != 0 {
        return Err(SandboxError::InvalidMetadata(
            "ABI-22 modules may not import mutable globals".into(),
        ));
    }
    if shape.imported_tables != 0 {
        return Err(SandboxError::InvalidMetadata(
            "ABI-22 modules may not import tables".into(),
        ));
    }
    if shape.defined_memories.len() != 1 || !shape.has_memory_section {
        return Err(SandboxError::InvalidMetadata(format!(
            "raw ABI-22 module must define exactly one work memory, got {}",
            shape.defined_memories.len()
        )));
    }
    if !shape.has_export_section {
        return Err(SandboxError::InvalidMetadata(
            "raw ABI-22 module must have an export section".into(),
        ));
    }
    if !shape.exports_function(abi::exports::DISPATCH) {
        return Err(SandboxError::MissingExport(abi::exports::DISPATCH.into()));
    }
    for name in [abi::exports::SHARED_MEMORY, abi::exports::LOCAL_MEMORY] {
        if shape.exports.contains_key(name) {
            return Err(SandboxError::InvalidMetadata(format!(
                "raw ABI-22 module already exports reserved memory name {name}"
            )));
        }
    }
    if shape
        .exports
        .keys()
        .any(|name| name.starts_with(MUTABLE_GLOBAL_EXPORT_PREFIX))
    {
        return Err(SandboxError::InvalidMetadata(
            "raw ABI-22 module already uses reserved mutable-global export names".into(),
        ));
    }
    let work_pages = shape.defined_memories[0].0;
    if !work_memory_fits_profile(work_pages, profile) {
        return Err(SandboxError::InvalidMetadata(format!(
            "work memory initial size {work_pages} pages exceeds the profile capacity"
        )));
    }
    let state_pages = pages_for_bytes(MAX_WASM_STATE_MEMORY_BYTES)?;
    if state_pages != 65 {
        return Err(SandboxError::InvalidMetadata(
            "ABI-22 state memory capacity must be exactly 65 pages".into(),
        ));
    }
    let work_max_pages = profile_work_max_pages(profile);
    if work_max_pages == 0
        || work_max_pages < work_pages
        || !prepared_work_capacity_fits_profile(work_max_pages, profile)
    {
        return Err(SandboxError::InvalidMetadata(
            "work memory cannot satisfy the prepared allocator reserve".into(),
        ));
    }
    if !aggregate_memory_fits_profile(work_max_pages, state_pages, profile) {
        return Err(SandboxError::InvalidMetadata(
            "ABI-22 memory aggregate exceeds the profile capacity".into(),
        ));
    }

    let mut module = wasm_encoder::Module::new();
    let mut reencoder = Finalizer {
        work_pages,
        work_max_pages,
        state_pages,
        mutable_globals: shape.mutable_global_indices().collect(),
        saw_memory: false,
        saw_export: false,
        seen_names: BTreeSet::new(),
    };
    reencoder
        .parse_core_module(&mut module, Parser::new(0), binary)
        .map_err(|error| SandboxError::compilation_failed(error.to_string()))?;
    if !reencoder.saw_memory || !reencoder.saw_export {
        return Err(SandboxError::InvalidMetadata(
            "finalizer could not locate memory/export sections".into(),
        ));
    }
    Ok(module.finish())
}

fn pages_for_bytes(bytes: usize) -> Result<u64, SandboxError> {
    if bytes == 0 {
        return Ok(0);
    }
    let bytes = u64::try_from(bytes)
        .map_err(|_| SandboxError::InvalidMetadata("memory capacity does not fit in u64".into()))?;
    Ok(bytes.div_ceil(65_536))
}

struct Finalizer {
    work_pages: u64,
    work_max_pages: u64,
    state_pages: u64,
    mutable_globals: Vec<u32>,
    saw_memory: bool,
    saw_export: bool,
    seen_names: BTreeSet<String>,
}

impl Finalizer {
    fn user_error(message: impl Into<String>) -> wasm_encoder::reencode::Error<String> {
        wasm_encoder::reencode::Error::UserError(message.into())
    }
}

impl Reencode for Finalizer {
    type Error = String;

    fn parse_memory_section(
        &mut self,
        memories: &mut wasm_encoder::MemorySection,
        section: wasmparser::MemorySectionReader<'_>,
    ) -> Result<(), wasm_encoder::reencode::Error<Self::Error>> {
        let entries: Vec<_> = section
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(wasm_encoder::reencode::Error::ParseError)?;
        if entries.len() != 1 {
            return Err(Self::user_error(
                "raw ABI-22 module must define one work memory",
            ));
        }
        let original = entries[0];
        if original.memory64 || original.shared || original.page_size_log2.is_some() {
            return Err(Self::user_error(
                "work memory must be a non-shared 32-bit default-page memory",
            ));
        }
        let work_pages = self.work_pages;
        if original.initial != work_pages {
            return Err(Self::user_error(format!(
                "work memory initial size changed during finalization: expected {}, got {}",
                work_pages, original.initial
            )));
        }
        memories.memory(MemoryType {
            minimum: work_pages,
            maximum: Some(self.work_max_pages),
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        for _ in 0..2 {
            memories.memory(MemoryType {
                minimum: self.state_pages,
                maximum: Some(self.state_pages),
                memory64: false,
                shared: false,
                page_size_log2: None,
            });
        }
        self.saw_memory = true;
        Ok(())
    }

    fn table_type(
        &mut self,
        table: wasmparser::TableType,
    ) -> Result<TableType, wasm_encoder::reencode::Error<Self::Error>> {
        if table.table64 || table.shared || table.initial > u64::from(MAX_WASM_TABLE_ELEMENTS) {
            return Err(Self::user_error(
                "table exceeds the fixed ABI-22 table contract",
            ));
        }
        Ok(TableType {
            element_type: self.ref_type(table.element_type)?,
            table64: false,
            minimum: table.initial,
            maximum: Some(table.initial),
            shared: false,
        })
    }

    fn parse_export(
        &mut self,
        exports: &mut wasm_encoder::ExportSection,
        export: wasmparser::Export<'_>,
    ) -> Result<(), wasm_encoder::reencode::Error<Self::Error>> {
        if !self.seen_names.insert(export.name.to_owned()) {
            return Err(Self::user_error(format!(
                "duplicate Wasm export name {}",
                export.name
            )));
        }
        if export.name == abi::exports::SHARED_MEMORY
            || export.name == abi::exports::LOCAL_MEMORY
            || export.name.starts_with(MUTABLE_GLOBAL_EXPORT_PREFIX)
        {
            return Err(Self::user_error(format!(
                "raw module uses reserved ABI-22 export name {}",
                export.name
            )));
        }
        wasm_encoder::reencode::utils::parse_export(self, exports, export)
    }

    fn parse_export_section(
        &mut self,
        exports: &mut wasm_encoder::ExportSection,
        section: wasmparser::ExportSectionReader<'_>,
    ) -> Result<(), wasm_encoder::reencode::Error<Self::Error>> {
        wasm_encoder::reencode::utils::parse_export_section(self, exports, section)?;
        exports.export(
            abi::exports::SHARED_MEMORY,
            wasm_encoder::ExportKind::Memory,
            1,
        );
        self.seen_names
            .insert(abi::exports::SHARED_MEMORY.to_owned());
        exports.export(
            abi::exports::LOCAL_MEMORY,
            wasm_encoder::ExportKind::Memory,
            2,
        );
        self.seen_names
            .insert(abi::exports::LOCAL_MEMORY.to_owned());
        for index in &self.mutable_globals {
            let name = mutable_global_name(*index);
            exports.export(&name, wasm_encoder::ExportKind::Global, *index);
            self.seen_names.insert(mutable_global_name(*index));
        }
        self.saw_export = true;
        Ok(())
    }
}

/// Validate the completed module's exact memory/global/table shape after
/// Wasmtime has parsed it with the ABI-22 feature set.
pub(crate) fn validate_finalized_shape(
    binary: &[u8],
    profile: &ExecutionProfile,
) -> Result<(), SandboxError> {
    let shape = inspect(binary)?;
    if shape.imported_memories != 0
        || shape.imported_globals != 0
        || shape.imported_tables != 0
        || shape.defined_memories.len() != 3
        || !shape.has_memory_section
        || !shape.has_export_section
        || profile.limits.max_memories != 3
    {
        return Err(SandboxError::InvalidMetadata(
            "module is not a complete ABI-22 three-memory artifact".into(),
        ));
    }
    let state_pages = pages_for_bytes(MAX_WASM_STATE_MEMORY_BYTES)?;
    let Some((work_pages, work_maximum)) = shape.defined_memories.first().copied() else {
        return Err(SandboxError::InvalidMetadata(
            "ABI-22 artifact is missing work memory".into(),
        ));
    };
    let Some(work_maximum) = work_maximum else {
        return Err(SandboxError::InvalidMetadata(
            "ABI-22 work memory must declare a bounded maximum".into(),
        ));
    };
    if work_pages == 0
        || work_pages > work_maximum
        || work_maximum != profile_work_max_pages(profile)
        || !work_memory_fits_profile(work_maximum, profile)
        || !prepared_work_capacity_fits_profile(work_maximum, profile)
    {
        return Err(SandboxError::InvalidMetadata(
            "ABI-22 work memory minimum/maximum exceeds the profile capacity".into(),
        ));
    }
    if shape.defined_memories
        != vec![
            (work_pages, Some(work_maximum)),
            (state_pages, Some(state_pages)),
            (state_pages, Some(state_pages)),
        ]
    {
        return Err(SandboxError::InvalidMetadata(
            "ABI-22 memories do not match one bounded work and two fixed 65-page state memories"
                .into(),
        ));
    }
    if !aggregate_memory_fits_profile(work_maximum, state_pages, profile) {
        return Err(SandboxError::InvalidMetadata(
            "ABI-22 memory aggregate exceeds the profile capacity".into(),
        ));
    }
    if shape.defined_tables.iter().any(|(initial, maximum)| {
        *initial > profile.limits.max_table_elements || maximum != &Some(*initial)
    }) || shape.defined_tables.len() as u64 > profile.limits.max_tables
    {
        return Err(SandboxError::InvalidMetadata(
            "ABI-22 tables must have fixed maximum sizes".into(),
        ));
    }
    let expected_memories = [
        ("memory", 0),
        (abi::exports::SHARED_MEMORY, 1),
        (abi::exports::LOCAL_MEMORY, 2),
    ];
    let exported_memory_count = shape
        .exports
        .values()
        .filter(|(kind, _)| *kind == ExternalKind::Memory)
        .count();
    if exported_memory_count != expected_memories.len()
        || expected_memories.iter().any(|(name, index)| {
            shape
                .exports
                .get(*name)
                .is_none_or(|(kind, exported_index)| {
                    *kind != ExternalKind::Memory || exported_index != index
                })
        })
    {
        return Err(SandboxError::InvalidMetadata(
            "ABI-22 work and state memory exports do not match their memories".into(),
        ));
    }
    let mutable_exports_valid = shape
        .exports
        .iter()
        .filter(|(name, _)| name.starts_with(MUTABLE_GLOBAL_EXPORT_PREFIX))
        .all(|(name, (kind, exported_index))| {
            let Some(index) = name
                .strip_prefix(MUTABLE_GLOBAL_EXPORT_PREFIX)
                .and_then(|suffix| suffix.parse::<u32>().ok())
            else {
                return false;
            };
            *kind == ExternalKind::Global
                && *exported_index == index
                && shape.defined_globals.get(index as usize) == Some(&true)
        });
    if !mutable_exports_valid
        || !shape.mutable_global_indices().all(|index| {
            shape
                .exports
                .get(&mutable_global_name(index))
                .is_some_and(|(kind, exported_index)| {
                    *kind == ExternalKind::Global && *exported_index == index
                })
        })
        || !shape.exports_function(abi::exports::DISPATCH)
    {
        return Err(SandboxError::InvalidMetadata(
            "ABI-22 mutable globals or dispatch export are incomplete".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_round_up_without_truncating() {
        assert_eq!(pages_for_bytes(0).unwrap(), 0);
        assert_eq!(pages_for_bytes(65_536).unwrap(), 1);
        assert_eq!(pages_for_bytes(65_537).unwrap(), 2);
    }

    #[test]
    fn finalized_shape_rejects_memory_alias_exports() {
        let binary = wat::parse_str(
            r#"(module
                (memory $work 1 1024)
                (memory $shared 65 65)
                (memory $local 65 65)
                (export "memory" (memory $work))
                (export "work_alias" (memory $work))
                (export "arena0_shared" (memory $shared))
                (export "arena0_local" (memory $local))
                (func (export "arena0_dispatch") (param i32 i32) (result i64)
                    i64.const 0))"#,
        )
        .unwrap();
        let error = validate_finalized_shape(&binary, &ExecutionProfile::current()).unwrap_err();
        assert!(matches!(error, SandboxError::InvalidMetadata(_)));
    }
}
