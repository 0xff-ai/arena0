//! Program schema types describing the agent-visible contract.
//!
//! Schemas are introspection only. Guests convert values with the concrete
//! DTOs' stock Serde and Borsh implementations. Hosts keep program values
//! opaque except for bounded, best-effort diagnostic projections of public
//! peer messages.

use std::io;

use borsh::schema::{BorshSchemaContainer, Definition, Fields};
use borsh::{BorshDeserialize, BorshSchema, BorshSerialize};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_BORSH_SCHEMA_BYTES: usize = 256 * 1024;
const MAX_DIAGNOSTIC_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_DIAGNOSTIC_DEPTH: usize = 64;
const MAX_DIAGNOSTIC_NODES: usize = 16_384;
const MAX_DIAGNOSTIC_JSON_BYTES: usize = 1024 * 1024;

/// A checked Borsh type schema used only to inspect opaque program values.
///
/// The encoded container keeps Borsh's unstable schema API behind an arena0
/// type. Schema-directed decoding is diagnostic and never participates in
/// execution, hashing, receipt construction, or verification.
#[derive(Serialize, BorshSerialize, Debug, Clone, PartialEq, Eq)]
#[serde(transparent)]
pub struct BorshSchemaDocument(Vec<u8>);

impl BorshSchemaDocument {
    /// Generate and validate the schema for a concrete program value.
    #[must_use]
    pub fn for_type<T: BorshSchema + ?Sized>() -> Self {
        let container = BorshSchemaContainer::for_type::<T>();
        let encoded = borsh::to_vec(&container).expect("Borsh schema serialization failed");
        Self::new(encoded).expect("derived Borsh schema must fit the metadata bound")
    }

    /// Construct a checked document from an encoded schema container.
    pub fn new(encoded: Vec<u8>) -> Result<Self, BorshSchemaDocumentError> {
        if encoded.len() > MAX_BORSH_SCHEMA_BYTES {
            return Err(BorshSchemaDocumentError::TooLarge {
                actual: encoded.len(),
                max: MAX_BORSH_SCHEMA_BYTES,
            });
        }
        let container: BorshSchemaContainer = borsh::from_slice(&encoded)
            .map_err(|error| BorshSchemaDocumentError::Invalid(error.to_string()))?;
        container
            .validate()
            .map_err(|error| BorshSchemaDocumentError::Invalid(format!("{error:?}")))?;
        Ok(Self(encoded))
    }

    /// Decode one Borsh value into a bounded diagnostic JSON projection.
    pub fn decode_json(&self, bytes: &[u8]) -> Result<Value, BorshDiagnosticError> {
        if bytes.len() > MAX_DIAGNOSTIC_MESSAGE_BYTES {
            return Err(BorshDiagnosticError::InputTooLarge {
                actual: bytes.len(),
                max: MAX_DIAGNOSTIC_MESSAGE_BYTES,
            });
        }
        let container: BorshSchemaContainer =
            borsh::from_slice(&self.0).expect("BorshSchemaDocument must remain valid");
        let mut input = bytes;
        let mut decoder = DiagnosticDecoder {
            container: &container,
            depth: 0,
            nodes: 0,
        };
        let value = decoder.decode(container.declaration(), &mut input)?;
        if !input.is_empty() {
            return Err(BorshDiagnosticError::TrailingBytes(input.len()));
        }
        let output_len = serde_json::to_vec(&value)
            .expect("serde_json::Value must serialize")
            .len();
        if output_len > MAX_DIAGNOSTIC_JSON_BYTES {
            return Err(BorshDiagnosticError::OutputTooLarge {
                actual: output_len,
                max: MAX_DIAGNOSTIC_JSON_BYTES,
            });
        }
        Ok(value)
    }
}

impl BorshDeserialize for BorshSchemaDocument {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let encoded = Vec::<u8>::deserialize_reader(reader)?;
        Self::new(encoded).map_err(io::Error::other)
    }
}

impl<'de> Deserialize<'de> for BorshSchemaDocument {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let encoded = <Vec<u8> as Deserialize>::deserialize(deserializer)?;
        Self::new(encoded).map_err(serde::de::Error::custom)
    }
}

/// Invalid embedded Borsh schema metadata.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BorshSchemaDocumentError {
    /// The encoded schema exceeds arena0's admission bound.
    #[error("Borsh schema is {actual} bytes; maximum is {max}")]
    TooLarge { actual: usize, max: usize },
    /// The schema cannot be decoded or violates Borsh's schema invariants.
    #[error("invalid Borsh schema: {0}")]
    Invalid(String),
}

/// Failure to produce a bounded diagnostic value from opaque Borsh bytes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BorshDiagnosticError {
    #[error("message is {actual} bytes; diagnostic maximum is {max}")]
    InputTooLarge { actual: usize, max: usize },
    #[error("diagnostic value exceeds maximum nesting depth")]
    TooDeep,
    #[error("diagnostic value exceeds maximum node count")]
    TooManyNodes,
    #[error("unexpected end of message")]
    UnexpectedEnd,
    #[error("unsupported Borsh declaration '{0}'")]
    Unsupported(String),
    #[error("invalid Borsh value: {0}")]
    InvalidValue(String),
    #[error("message has {0} trailing bytes")]
    TrailingBytes(usize),
    #[error("diagnostic JSON is {actual} bytes; maximum is {max}")]
    OutputTooLarge { actual: usize, max: usize },
}

struct DiagnosticDecoder<'a> {
    container: &'a BorshSchemaContainer,
    depth: usize,
    nodes: usize,
}

impl DiagnosticDecoder<'_> {
    fn decode(
        &mut self,
        declaration: &str,
        input: &mut &[u8],
    ) -> Result<Value, BorshDiagnosticError> {
        if self.depth >= MAX_DIAGNOSTIC_DEPTH {
            return Err(BorshDiagnosticError::TooDeep);
        }
        self.nodes = self.nodes.saturating_add(1);
        if self.nodes > MAX_DIAGNOSTIC_NODES {
            return Err(BorshDiagnosticError::TooManyNodes);
        }
        self.depth += 1;
        let result = self.decode_inner(declaration, input);
        self.depth -= 1;
        result
    }

    fn decode_inner(
        &mut self,
        declaration: &str,
        input: &mut &[u8],
    ) -> Result<Value, BorshDiagnosticError> {
        let definition = self
            .container
            .get_definition(declaration)
            .cloned()
            .ok_or_else(|| BorshDiagnosticError::Unsupported(declaration.to_owned()))?;
        match definition {
            Definition::Primitive(width) => self.decode_primitive(declaration, width, input),
            Definition::Sequence {
                length_width,
                length_range,
                elements,
            } => {
                let length = if length_width == 0 {
                    *length_range.start()
                } else {
                    read_unsigned(input, length_width)?
                };
                if !length_range.contains(&length) {
                    return Err(BorshDiagnosticError::InvalidValue(format!(
                        "sequence length {length} is outside {length_range:?}"
                    )));
                }
                let length = usize::try_from(length).map_err(|_| {
                    BorshDiagnosticError::InvalidValue("sequence length does not fit usize".into())
                })?;
                if declaration == "String" {
                    let bytes = take(input, length)?;
                    let text = std::str::from_utf8(bytes).map_err(|error| {
                        BorshDiagnosticError::InvalidValue(format!("invalid UTF-8 string: {error}"))
                    })?;
                    return Ok(Value::String(text.to_owned()));
                }
                if elements == "u8" {
                    return Ok(Value::String(format!(
                        "0x{}",
                        hex::encode(take(input, length)?)
                    )));
                }
                if self.nodes.saturating_add(length) > MAX_DIAGNOSTIC_NODES {
                    return Err(BorshDiagnosticError::TooManyNodes);
                }
                let mut values = Vec::with_capacity(length.min(4096));
                for _ in 0..length {
                    values.push(self.decode(&elements, input)?);
                }
                Ok(Value::Array(values))
            }
            Definition::Tuple { elements } => {
                let mut values = Vec::with_capacity(elements.len());
                for element in elements {
                    values.push(self.decode(&element, input)?);
                }
                Ok(Value::Array(values))
            }
            Definition::Struct { fields } => self.decode_fields(fields, input),
            Definition::Enum {
                tag_width,
                variants,
            } => {
                if tag_width == 0 {
                    return Err(BorshDiagnosticError::Unsupported(format!(
                        "untagged enum {declaration}"
                    )));
                }
                let tag = read_unsigned(input, tag_width)? as i64;
                let (_, variant, payload) = variants
                    .into_iter()
                    .find(|(discriminant, _, _)| *discriminant == tag)
                    .ok_or_else(|| {
                        BorshDiagnosticError::InvalidValue(format!(
                            "unknown {declaration} variant tag {tag}"
                        ))
                    })?;
                let value = self.decode(&payload, input)?;
                if declaration.starts_with("Option<") {
                    return match variant.as_str() {
                        "None" => Ok(Value::Null),
                        "Some" => Ok(value),
                        _ => Err(BorshDiagnosticError::InvalidValue(format!(
                            "invalid Option variant {variant}"
                        ))),
                    };
                }
                if value.is_null() {
                    Ok(Value::String(variant))
                } else {
                    Ok(Value::Object([(variant, value)].into_iter().collect()))
                }
            }
        }
    }

    fn decode_fields(
        &mut self,
        fields: Fields,
        input: &mut &[u8],
    ) -> Result<Value, BorshDiagnosticError> {
        match fields {
            Fields::Empty => Ok(Value::Null),
            Fields::UnnamedFields(fields) if fields.len() == 1 => self.decode(&fields[0], input),
            Fields::UnnamedFields(fields) => {
                let mut values = Vec::with_capacity(fields.len());
                for field in fields {
                    values.push(self.decode(&field, input)?);
                }
                Ok(Value::Array(values))
            }
            Fields::NamedFields(fields) => {
                let mut values = serde_json::Map::with_capacity(fields.len());
                for (name, declaration) in fields {
                    values.insert(name, self.decode(&declaration, input)?);
                }
                Ok(Value::Object(values))
            }
        }
    }

    fn decode_primitive(
        &self,
        declaration: &str,
        width: u8,
        input: &mut &[u8],
    ) -> Result<Value, BorshDiagnosticError> {
        let bytes = take(input, usize::from(width))?;
        let number = |value: u64| Value::Number(value.into());
        Ok(match declaration {
            "()" if width == 0 => Value::Null,
            "bool" if width == 1 => match bytes[0] {
                0 => Value::Bool(false),
                1 => Value::Bool(true),
                value => {
                    return Err(BorshDiagnosticError::InvalidValue(format!(
                        "invalid bool byte {value}"
                    )));
                }
            },
            "u8" if width == 1 => number(u64::from(bytes[0])),
            "u16" if width == 2 => number(u64::from(u16::from_le_bytes(bytes.try_into().unwrap()))),
            "u32" if width == 4 => number(u64::from(u32::from_le_bytes(bytes.try_into().unwrap()))),
            "u64" if width == 8 => number(u64::from_le_bytes(bytes.try_into().unwrap())),
            "i8" if width == 1 => Value::Number(i64::from(bytes[0] as i8).into()),
            "i16" if width == 2 => {
                Value::Number(i64::from(i16::from_le_bytes(bytes.try_into().unwrap())).into())
            }
            "i32" if width == 4 => {
                Value::Number(i64::from(i32::from_le_bytes(bytes.try_into().unwrap())).into())
            }
            "i64" if width == 8 => {
                Value::Number(i64::from_le_bytes(bytes.try_into().unwrap()).into())
            }
            "u128" if width == 16 => {
                Value::String(u128::from_le_bytes(bytes.try_into().unwrap()).to_string())
            }
            "i128" if width == 16 => {
                Value::String(i128::from_le_bytes(bytes.try_into().unwrap()).to_string())
            }
            "f32" if width == 4 => {
                finite_float(f64::from(f32::from_le_bytes(bytes.try_into().unwrap())))?
            }
            "f64" if width == 8 => finite_float(f64::from_le_bytes(bytes.try_into().unwrap()))?,
            _ => return Err(BorshDiagnosticError::Unsupported(declaration.to_owned())),
        })
    }
}

fn take<'a>(input: &mut &'a [u8], length: usize) -> Result<&'a [u8], BorshDiagnosticError> {
    if input.len() < length {
        return Err(BorshDiagnosticError::UnexpectedEnd);
    }
    let (value, rest) = input.split_at(length);
    *input = rest;
    Ok(value)
}

fn read_unsigned(input: &mut &[u8], width: u8) -> Result<u64, BorshDiagnosticError> {
    let bytes = take(input, usize::from(width))?;
    let mut value = [0_u8; 8];
    if bytes.len() > value.len() {
        return Err(BorshDiagnosticError::InvalidValue(format!(
            "integer width {width} exceeds 8 bytes"
        )));
    }
    value[..bytes.len()].copy_from_slice(bytes);
    Ok(u64::from_le_bytes(value))
}

fn finite_float(value: f64) -> Result<Value, BorshDiagnosticError> {
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .ok_or_else(|| BorshDiagnosticError::InvalidValue("non-finite float".into()))
}

/// A standard JSON Schema Draft 2020-12 document.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(transparent)]
pub struct JsonSchemaDocument(Value);

impl JsonSchemaDocument {
    /// Construct a checked Draft 2020-12 schema document.
    pub fn new(value: Value) -> Result<Self, JsonSchemaDocumentError> {
        let Some(object) = value.as_object() else {
            return Err(JsonSchemaDocumentError::NotObject);
        };
        match object.get("$schema").and_then(Value::as_str) {
            Some(schemars::consts::meta_schemas::DRAFT2020_12) => Ok(Self(value)),
            _ => Err(JsonSchemaDocumentError::WrongDraft),
        }
    }

    /// Generate a Draft 2020-12 document for a Rust type.
    #[must_use]
    pub fn for_type<T: schemars::JsonSchema>() -> Self {
        let generator = schemars::generate::SchemaSettings::draft2020_12().into_generator();
        let schema = generator.into_root_schema_for::<T>();
        let value = serde_json::to_value(schema).expect("JSON Schema serialization failed");
        Self::new(value).expect("Schemars must emit a Draft 2020-12 root schema")
    }

    /// The public schema for the unit value.
    #[must_use]
    pub fn unit() -> Self {
        Self::for_type::<()>()
    }

    /// Borrow the JSON Schema value.
    #[must_use]
    pub fn as_value(&self) -> &Value {
        &self.0
    }

    /// Check that every reference stays within this document's root `$defs`.
    pub fn validate_local_references(&self) -> Result<(), JsonSchemaDocumentError> {
        let mut nodes = vec![&self.0];
        while let Some(node) = nodes.pop() {
            if let Some(reference) = node.get("$ref").and_then(Value::as_str) {
                if !reference.starts_with("#/$defs/") {
                    return Err(JsonSchemaDocumentError::ExternalReference(
                        reference.to_string(),
                    ));
                }
                if self.0.pointer(&reference[1..]).is_none() {
                    return Err(JsonSchemaDocumentError::UnresolvedReference(
                        reference.to_string(),
                    ));
                }
            }
            if let Some(object) = node.as_object() {
                nodes.extend(object.values());
            } else if let Some(array) = node.as_array() {
                nodes.extend(array);
            }
        }
        Ok(())
    }
}

impl BorshSerialize for JsonSchemaDocument {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        let bytes = serde_json::to_vec(&self.0).map_err(io::Error::other)?;
        BorshSerialize::serialize(&bytes, writer)
    }
}

impl BorshDeserialize for JsonSchemaDocument {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let bytes = Vec::<u8>::deserialize_reader(reader)?;
        let value = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        Self::new(value).map_err(io::Error::other)
    }
}

/// A malformed or unsupported JSON Schema document.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JsonSchemaDocumentError {
    /// Program schemas must be JSON objects so they can name their draft.
    #[error("program JSON Schema must be an object")]
    NotObject,
    /// Arena0 accepts one explicit JSON Schema draft.
    #[error("program JSON Schema must declare Draft 2020-12")]
    WrongDraft,
    /// Program schemas cannot fetch or depend on an external resource.
    #[error("external JSON Schema reference is not allowed: {0}")]
    ExternalReference(String),
    /// A local reference must name a node in this document.
    #[error("unresolved JSON Schema reference: {0}")]
    UnresolvedReference(String),
}

/// Top-level schema for a program.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct ProgramSchema {
    /// Schema for the program's persistent state.
    pub state: StateSchema,
    /// Callout variants the program can request from agents.
    pub callouts: Vec<CalloutSchema>,
    /// Message types the program sends/receives between peers.
    pub messages: Vec<MessageSchema>,
    /// Schema for program initialization parameters.
    pub params: JsonSchemaDocument,
    /// Query endpoints the program exposes for read-only state inspection.
    pub queries: Vec<QuerySchema>,
    /// The program's derived terminal outcome.
    pub outcome: JsonSchemaDocument,
}

/// Schema for a program's persistent state region.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct StateSchema {
    /// Type layout of the state.
    pub schema: JsonSchemaDocument,
    /// Maximum serialized state size in bytes.
    pub max_bytes: u32,
}

/// Schema for one callout variant that a program can request from agents.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct CalloutSchema {
    /// Machine-readable variant name.
    pub name: String,
    /// Human-facing prompt describing what the agent should do.
    pub prompt: String,
    /// Type of the context data sent with the callout request.
    pub input: JsonSchemaDocument,
    /// Type of the response the agent should return.
    pub output: JsonSchemaDocument,
}

/// Schema for one peer-to-peer message type.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct MessageSchema {
    /// Borsh layout of the program's complete peer-message type.
    pub borsh: BorshSchemaDocument,
}

/// Schema for a read-only query endpoint.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct QuerySchema {
    /// Machine-readable query name.
    pub name: String,
    /// Human-facing label for the query.
    pub label: String,
    /// Type of the query request payload.
    pub request: JsonSchemaDocument,
    /// Type of the query response payload.
    pub response: JsonSchemaDocument,
}

/// Routing metadata for a state-machine primitive field.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct PrimitiveRouteSchema {
    /// Shared field name that owns the primitive state.
    pub field: String,
    /// Primitive type name, such as `CommitReveal`.
    pub primitive: String,
    /// Program peer-message constructor used by generated `.send_to(...)`.
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(BorshSerialize, BorshSchema)]
    enum DiagnosticMessage {
        Ready,
        Offer {
            amount: u128,
            digest: [u8; 4],
            note: Option<String>,
            scores: Vec<u16>,
        },
    }

    #[test]
    fn borsh_schema_decodes_a_nested_program_message_for_diagnostics() {
        let schema = BorshSchemaDocument::for_type::<DiagnosticMessage>();
        let bytes = borsh::to_vec(&DiagnosticMessage::Offer {
            amount: u128::MAX,
            digest: [0xde, 0xad, 0xbe, 0xef],
            note: Some("final".into()),
            scores: vec![3, 5],
        })
        .unwrap();

        assert_eq!(
            schema.decode_json(&bytes).unwrap(),
            serde_json::json!({
                "Offer": {
                    "amount": u128::MAX.to_string(),
                    "digest": "0xdeadbeef",
                    "note": "final",
                    "scores": [3, 5]
                }
            })
        );

        let ready = borsh::to_vec(&DiagnosticMessage::Ready).unwrap();
        assert_eq!(
            schema.decode_json(&ready).unwrap(),
            Value::String("Ready".into())
        );
    }

    #[test]
    fn borsh_diagnostic_decode_rejects_invalid_or_unbounded_values() {
        let schema = BorshSchemaDocument::for_type::<DiagnosticMessage>();
        assert!(matches!(
            schema.decode_json(&[99]),
            Err(BorshDiagnosticError::InvalidValue(_))
        ));

        let ready_with_trailing_byte = [0, 0];
        assert_eq!(
            schema.decode_json(&ready_with_trailing_byte),
            Err(BorshDiagnosticError::TrailingBytes(1))
        );

        let string_schema = BorshSchemaDocument::for_type::<String>();
        let large_escaped_string = "\0".repeat(MAX_DIAGNOSTIC_JSON_BYTES / 5);
        let bytes = borsh::to_vec(&large_escaped_string).unwrap();
        assert!(matches!(
            string_schema.decode_json(&bytes),
            Err(BorshDiagnosticError::OutputTooLarge { .. })
        ));

        assert!(serde_json::from_value::<BorshSchemaDocument>(serde_json::json!([0])).is_err());
    }

    #[test]
    fn json_schema_document_borsh_round_trip() {
        let document = JsonSchemaDocument::new(serde_json::json!({
            "$schema": schemars::consts::meta_schemas::DRAFT2020_12,
            "type": "object",
            "properties": { "answer": { "type": "integer" } }
        }))
        .unwrap();

        let bytes = borsh::to_vec(&document).unwrap();
        let decoded = JsonSchemaDocument::try_from_slice(&bytes).unwrap();

        assert_eq!(decoded, document);
    }

    #[test]
    fn json_schema_document_rejects_an_implicit_draft() {
        let error = JsonSchemaDocument::new(serde_json::json!({ "type": "null" })).unwrap_err();
        assert_eq!(error, JsonSchemaDocumentError::WrongDraft);
    }

    #[test]
    fn json_schema_references_must_resolve_inside_root_defs() {
        let local = JsonSchemaDocument::new(serde_json::json!({
            "$schema": schemars::consts::meta_schemas::DRAFT2020_12,
            "$defs": {"Count": {"type": "integer"}},
            "$ref": "#/$defs/Count"
        }))
        .unwrap();
        assert!(local.validate_local_references().is_ok());

        let external = JsonSchemaDocument::new(serde_json::json!({
            "$schema": schemars::consts::meta_schemas::DRAFT2020_12,
            "$ref": "https://example.invalid/count.json"
        }))
        .unwrap();
        assert!(matches!(
            external.validate_local_references(),
            Err(JsonSchemaDocumentError::ExternalReference(_))
        ));

        let missing = JsonSchemaDocument::new(serde_json::json!({
            "$schema": schemars::consts::meta_schemas::DRAFT2020_12,
            "$ref": "#/$defs/Missing"
        }))
        .unwrap();
        assert!(matches!(
            missing.validate_local_references(),
            Err(JsonSchemaDocumentError::UnresolvedReference(_))
        ));
    }
}
