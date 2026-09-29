//! Decoding of trace messages with the program's Borsh message schema.

use arena0_program::BorshSchemaDocument;
use serde_json::Value;

/// Decode one agreed message. Exactly one of the two results is `Some`.
/// Failures are per step and never fatal; the error text describes the
/// schema mismatch, never the payload.
pub(crate) fn decode_message(
    schema: Option<&BorshSchemaDocument>,
    data: &[u8],
) -> (Option<Value>, Option<String>) {
    let Some(schema) = schema else {
        return (None, Some("program declares no message schema".to_owned()));
    };
    match schema.decode_json(data) {
        Ok(value) => (Some(value), None),
        Err(error) => (None, Some(error.to_string())),
    }
}
