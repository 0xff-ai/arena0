//! Immutable program artifacts and their private metadata-section codec.
//!
//! Completed artifacts are parse-only: building a section-less Wasm module is
//! the only path that executes the bounded metadata export probe.

use std::sync::Arc;

use arena0_program::{MAX_METADATA_BYTES, PROGRAM_MAX_LEN, ProgramDefinition, ProgramHash};
use wasmparser::{Parser, Payload};

use crate::{SandboxError, validation};

/// One immutable Wasm program and its content address.
///
/// Construction computes the hash and parses metadata once. Clones share both
/// the exact bytes and definition.
#[derive(Clone)]
pub struct Program {
    bytes: Arc<[u8]>,
    hash: ProgramHash,
    definition: Arc<ProgramDefinition>,
}

impl TryFrom<Vec<u8>> for Program {
    type Error = SandboxError;

    fn try_from(bytes: Vec<u8>) -> Result<Self, Self::Error> {
        Self::parse(bytes)
    }
}

impl TryFrom<Arc<[u8]>> for Program {
    type Error = SandboxError;

    fn try_from(bytes: Arc<[u8]>) -> Result<Self, Self::Error> {
        Self::parse(bytes)
    }
}

impl TryFrom<&[u8]> for Program {
    type Error = SandboxError;

    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        Self::parse(bytes.to_vec())
    }
}

impl Program {
    /// Parse and own an exact Wasm artifact and its embedded definition.
    pub fn parse(bytes: impl Into<Arc<[u8]>>) -> Result<Self, SandboxError> {
        let bytes = bytes.into();
        if bytes.len() as u64 > PROGRAM_MAX_LEN {
            return Err(SandboxError::ProgramTooLarge {
                size: bytes.len() as u64,
                max: PROGRAM_MAX_LEN,
            });
        }
        let hash = ProgramHash::of(&bytes);
        let definition = Arc::new(Self::read_metadata(&bytes)?);
        Ok(Self {
            bytes,
            hash,
            definition,
        })
    }

    /// Embed one definition and return the resulting validated artifact.
    pub(crate) fn embed(wasm: &[u8], definition: &ProgramDefinition) -> Result<Self, SandboxError> {
        let metadata = definition
            .encode()
            .map_err(|error| SandboxError::SerializationFailed(error.to_string()))?;
        Self::parse(Self::append_metadata(wasm, &metadata))
    }

    /// The exact content-addressed Wasm bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Blake3 identity of [`Self::bytes`].
    #[must_use]
    pub const fn hash(&self) -> ProgramHash {
        self.hash
    }

    /// The metadata parsed when this artifact was built.
    #[must_use]
    pub fn definition(&self) -> &ProgramDefinition {
        &self.definition
    }

    fn read_metadata(binary: &[u8]) -> Result<ProgramDefinition, SandboxError> {
        let mut found = None;
        for payload in Parser::new(0).parse_all(binary) {
            let payload = payload.map_err(|e| SandboxError::compilation_failed(e.to_string()))?;
            if let Payload::CustomSection(section) = payload
                && section.name() == METADATA_SECTION
            {
                if section.data().len() > MAX_METADATA_BYTES as usize {
                    return Err(SandboxError::InvalidMetadata(format!(
                        "metadata section exceeds {} bytes",
                        MAX_METADATA_BYTES
                    )));
                }
                if found.is_some() {
                    return Err(SandboxError::InvalidMetadata(
                        "duplicate arena0.metadata custom sections".into(),
                    ));
                }
                let definition = ProgramDefinition::decode(section.data())
                    .map_err(|error| SandboxError::InvalidMetadata(error.to_string()))?;
                validation::validate_program_definition(&definition)?;
                found = Some(definition);
            }
        }
        found.ok_or(SandboxError::MissingMetadata)
    }

    fn append_metadata(binary: &[u8], data: &[u8]) -> Vec<u8> {
        let mut out = binary.to_vec();
        // Custom section: id 0x00, then LEB128(size of name_len + name + data).
        out.push(0x00);
        let mut payload = Vec::with_capacity(METADATA_SECTION.len() + data.len() + 2);
        push_leb128(&mut payload, METADATA_SECTION.len() as u64);
        payload.extend_from_slice(METADATA_SECTION.as_bytes());
        payload.extend_from_slice(data);
        push_leb128(&mut out, payload.len() as u64);
        out.extend_from_slice(&payload);
        out
    }
}

impl std::fmt::Debug for Program {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Program")
            .field("hash", &self.hash)
            .field("len", &self.bytes.len())
            .finish()
    }
}

const METADATA_SECTION: &str = "arena0.metadata";

fn push_leb128(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use super::*;

    use arena0_program::{JsonSchemaDocument, ProgramMetadata, ProgramSchema, StateSchema};

    use crate::WasmtimeEngine;

    fn unit_schema() -> JsonSchemaDocument {
        JsonSchemaDocument::new(serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "null"
        }))
        .unwrap()
    }

    fn schema() -> ProgramSchema {
        let unit = unit_schema();
        ProgramSchema {
            state: StateSchema {
                schema: unit.clone(),
                max_bytes: 0,
            },
            callouts: Vec::new(),
            messages: Vec::new(),
            params: unit.clone(),
            queries: Vec::new(),
            outcome: unit,
        }
    }

    fn definition() -> ProgramDefinition {
        ProgramDefinition {
            metadata: ProgramMetadata {
                name: "round-trip".into(),
                version: "0.1.0".into(),
                description: "section test".into(),
                author: None,
                capabilities: Vec::new(),
                display_name: "Round trip".into(),
                participants: arena0_program::ParticipantCount::Exact { count: 2 },
            },
            schema: schema(),
        }
    }

    #[test]
    fn invalid_metadata_sections_are_rejected() {
        let encoded = definition().encode().unwrap();
        let once = Program::append_metadata(b"\0asm\x01\x00\x00\x00", &encoded);

        let mut wrong_magic = encoded.clone();
        wrong_magic[0] ^= 1;
        let mut unsupported_version = encoded.clone();
        unsupported_version[4..8].copy_from_slice(&2u32.to_le_bytes());
        let mut definition = definition();
        definition.schema.params = JsonSchemaDocument::new(serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": 7
        }))
        .unwrap();

        let cases = [
            (
                "duplicate section",
                Program::append_metadata(&once, &encoded),
                "duplicate",
            ),
            (
                "wrong definition magic",
                Program::append_metadata(b"\0asm\x01\x00\x00\x00", &wrong_magic),
                "wrong magic",
            ),
            (
                "unsupported definition version",
                Program::append_metadata(b"\0asm\x01\x00\x00\x00", &unsupported_version),
                "unsupported program definition version 2",
            ),
            (
                "invalid JSON schema",
                Program::append_metadata(b"\0asm\x01\x00\x00\x00", &definition.encode().unwrap()),
                "params JSON Schema is invalid",
            ),
        ];
        for (case, wasm, expected) in cases {
            let error = Program::read_metadata(&wasm).unwrap_err();
            assert!(
                matches!(&error, SandboxError::InvalidMetadata(message) if message.contains(expected)),
                "{case}: {error}"
            );
        }
    }

    #[test]
    fn participant_counts_are_bounded() {
        for participants in [
            arena0_program::ParticipantCount::Exact { count: 0 },
            arena0_program::ParticipantCount::Exact { count: 1 },
            arena0_program::ParticipantCount::Exact { count: 65 },
            arena0_program::ParticipantCount::Exact { count: u8::MAX },
            arena0_program::ParticipantCount::Range { min: 1, max: 64 },
            arena0_program::ParticipantCount::Range { min: 4, max: 3 },
            arena0_program::ParticipantCount::Range { min: 2, max: 65 },
        ] {
            let mut definition = definition();
            definition.metadata.participants = participants;
            let wasm =
                Program::append_metadata(b"\0asm\x01\x00\x00\x00", &definition.encode().unwrap());

            let error = Program::read_metadata(&wasm).unwrap_err();
            assert!(
                matches!(&error, SandboxError::InvalidMetadata(message) if message.contains("participant count")),
                "unexpected error for {participants}: {error}"
            );
        }
    }

    #[test]
    fn loading_requires_metadata_section() {
        let error = crate::Program::parse(metadata_export_module()).unwrap_err();

        assert!(matches!(error, SandboxError::MissingMetadata));
    }

    fn wat_data(bytes: &[u8]) -> String {
        bytes
            .iter()
            .map(|byte| format!("\\{byte:02x}"))
            .collect::<Vec<_>>()
            .join("")
    }

    fn metadata_export_module() -> Vec<u8> {
        let metadata = definition().encode().unwrap();
        let metadata_data = wat_data(&metadata);
        let wat = format!(
            r#"
            (module
              (memory (export "memory") 1)
              (global (export "arena0_abi_version") i32 (i32.const 20))
              (data (i32.const 64) "{metadata_data}")
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
              (func (export "arena0_alloc") (param i32) (result i32) i32.const 2048)
              (func (export "arena0_dealloc") (param i32 i32))
              (func (export "arena0_initialize") (param i32 i32) (result i64) i64.const 0)
              (func (export "arena0_shared") (param i32 i32) (result i64) i64.const 0)
              (func (export "arena0_local") (param i32 i32) (result i64) i64.const 0)
              (func (export "arena0_writer") (param i32 i32) (result i64) i64.const 0)
              (func (export "arena0_outcome") (param i32 i32) (result i64) i64.const 0)
              (func (export "arena0_query") (param i32 i32) (result i64) i64.const 0)
              (func (export "arena0_view") (param i32 i32) (result i64) i64.const 0)
              (func (export "arena0_metadata") (result i64)
                i32.const 64
                i32.const {metadata_len}
                call $pack))
            "#,
            metadata_len = metadata.len()
        );
        wat::parse_str(wat).unwrap()
    }

    #[test]
    fn embedding_exports_a_versioned_definition() {
        let wasm = metadata_export_module();
        let engine = WasmtimeEngine::new().unwrap();
        let embedded = engine.build_program(&wasm).unwrap().bytes().to_vec();

        assert_ne!(embedded, wasm);
        assert_eq!(Program::read_metadata(&embedded).unwrap(), definition());
        assert_eq!(engine.build_program(&embedded).unwrap().bytes(), embedded);
    }

    #[test]
    fn admission_reuses_the_engine_scoped_compiled_program() {
        let engine = WasmtimeEngine::new().unwrap();
        let program = engine.build_program(&metadata_export_module()).unwrap();
        let output = SharedWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(output.clone())
            .finish();

        let (first, second) = tracing::subscriber::with_default(subscriber, || {
            (
                engine.admit(&program).unwrap(),
                engine.admit(&program).unwrap(),
            )
        });

        assert!(Arc::ptr_eq(&first, &second));

        let lines = output
            .text()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["fields"]["result_class"], "compiled");
        assert_eq!(lines[1]["fields"]["result_class"], "cached");
        let allowed = [
            "operation",
            "version",
            "encoded_size",
            "success",
            "result_class",
            "elapsed_us",
        ];
        for line in lines {
            assert_eq!(line["target"], "arena0::performance");
            let fields = line["fields"].as_object().unwrap();
            assert!(fields.keys().all(|field| allowed.contains(&field.as_str())));
            for forbidden in [
                "params",
                "outcome",
                "context",
                "signature",
                "program",
                "private",
                "sql",
                "payload",
            ] {
                assert!(!fields.contains_key(forbidden));
            }
        }
    }

    #[test]
    fn persistent_compilation_cache_survives_engine_restart() {
        let builder = WasmtimeEngine::new().unwrap();
        let program = builder.build_program(&metadata_export_module()).unwrap();
        let cache_dir = tempfile::tempdir().unwrap();

        let first = WasmtimeEngine::new_persistent(cache_dir.path()).unwrap();
        first.admit(&program).unwrap();
        assert_eq!(first.persistent_cache.as_ref().unwrap().cache_misses(), 1);
        drop(first);

        let second = WasmtimeEngine::new_persistent(cache_dir.path()).unwrap();
        second.admit(&program).unwrap();
        assert_eq!(second.persistent_cache.as_ref().unwrap().cache_hits(), 1);
    }

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl SharedWriter {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    struct SharedWriterGuard(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriterGuard {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedWriter {
        type Writer = SharedWriterGuard;

        fn make_writer(&'a self) -> Self::Writer {
            SharedWriterGuard(Arc::clone(&self.0))
        }
    }
}
