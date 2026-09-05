//! JSON-Schema checks and defaults at the agent boundary.
//!
//! The host validates agent JSON against the program's public schema (an input
//! check, not a serialization rule) and forwards the bytes unchanged. The
//! guest's generated code owns every conversion to and from concrete DTOs.

use arena0_api::{ApiError, ApiErrorCode};
use arena0_program::JsonSchemaDocument;
use serde_json::Value;

/// Prepare one agent JSON field for the guest.
///
/// If the field is absent, the schema supplies a default only when the root
/// value has a true empty form:
/// `null`, an empty object, or an empty array. Scalar and enum values do not
/// get invented defaults. Typed values are re-serialized canonically; the
/// guest parses them with its own Serde impl.
pub(crate) fn encode_json_field(
    schema: &JsonSchemaDocument,
    value: Option<&Value>,
    field: &str,
) -> Result<Vec<u8>, ApiError> {
    let validator = jsonschema::validator_for(schema.as_value()).map_err(|error| {
        ApiError::new(
            ApiErrorCode::Internal,
            format!("`{field}` schema is invalid JSON Schema: {error}"),
        )
    })?;
    let owned = value.cloned().unwrap_or_else(|| {
        [
            Value::Null,
            Value::Object(Default::default()),
            Value::Array(Vec::new()),
        ]
        .into_iter()
        .find(|candidate| validator.is_valid(candidate))
        .unwrap_or(Value::Null)
    });
    validator.validate(&owned).map_err(|error| {
        let path = error.instance_path().as_str();
        let path = if path.is_empty() { "$" } else { path };
        ApiError::new(
            ApiErrorCode::Schema,
            format!("`{field}`: JSON Schema validation failed at {path}: {error}"),
        )
    })?;
    serde_json::to_vec(&owned)
        .map_err(|e| ApiError::new(ApiErrorCode::BadRequest, format!("`{field}`: {e}")))
}

/// Parse guest-produced JSON at the host boundary.
pub(crate) fn decode_guest_json(bytes: &[u8], field: &str) -> Result<Value, ApiError> {
    serde_json::from_slice(bytes).map_err(|error| {
        ApiError::new(
            ApiErrorCode::Execution,
            format!("guest returned invalid JSON for `{field}`: {error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_guest_json_is_an_execution_error() {
        let error = decode_guest_json(&[0xff], "outcome").unwrap_err();
        assert_eq!(error.code, ApiErrorCode::Execution);
        assert!(error.message.contains("invalid JSON for `outcome`"));
    }

    #[test]
    fn agent_json_is_validated_and_forwarded_as_json() {
        let schema = JsonSchemaDocument::new(serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": { "rounds": { "type": "integer" } },
            "required": ["rounds"],
            "additionalProperties": false
        }))
        .unwrap();
        let value = serde_json::json!({"rounds": 3});
        let encoded = encode_json_field(&schema, Some(&value), "params").unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&encoded).unwrap(), value);

        let error = encode_json_field(
            &schema,
            Some(&serde_json::json!({"rounds": "three"})),
            "params",
        )
        .unwrap_err();
        assert_eq!(error.code, ApiErrorCode::Schema);
    }
}
