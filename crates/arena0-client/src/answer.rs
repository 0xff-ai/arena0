//! Agent answer and `--param` assembly.
//!
//! Clients preserve JSON values and leave schema validation to the daemon, the
//! boundary that owns program admission and execution.

use serde_json::{Map, Value};

/// Build the params JSON object from `--param KEY=VALUE` pairs. Each value is
/// parsed as JSON when possible and otherwise kept as a string. `None` means
/// the caller supplied no params.
pub fn assemble_params(pairs: &[String]) -> Result<Option<Value>, String> {
    let pairs = split_pairs(pairs)?;
    params_from_pairs(&pairs)
}

fn split_pairs(pairs: &[String]) -> Result<Vec<(String, String)>, String> {
    pairs
        .iter()
        .map(|pair| {
            let (key, raw) = pair
                .split_once('=')
                .ok_or_else(|| format!("--param must be KEY=VALUE, got '{pair}'"))?;
            if key.is_empty() {
                return Err(format!("--param has an empty key: '{pair}'"));
            }
            Ok((key.to_string(), raw.to_string()))
        })
        .collect()
}

/// Build a params JSON object from already-split `(key, value)` pairs.
pub fn params_from_pairs(pairs: &[(String, String)]) -> Result<Option<Value>, String> {
    if pairs.is_empty() {
        return Ok(None);
    }
    let mut map = Map::new();
    for (key, raw) in pairs {
        if key.is_empty() {
            return Err("--param has an empty key".into());
        }
        map.insert(key.clone(), scalar(raw));
    }
    Ok(Some(Value::Object(map)))
}

/// Parse text as JSON when possible and otherwise preserve it as a JSON string.
#[must_use]
pub fn scalar(raw: &str) -> Value {
    serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// Render a decoded outcome as a short human string. This is value-driven and
/// falls back to compact JSON for shapes that are not a single external variant.
#[must_use]
pub fn describe_outcome(value: &Value) -> String {
    if let Some(s) = value.as_str() {
        return s.to_string();
    }
    let Some(obj) = value.as_object() else {
        return value.to_string();
    };
    if obj.len() == 1
        && let Some((variant, fields)) = obj.iter().next()
    {
        match fields {
            Value::Null => return variant.clone(),
            Value::Object(m) => {
                let parts: Vec<String> = m
                    .iter()
                    .map(|(k, v)| format!("{k} {}", outcome_field(k, v)))
                    .collect();
                return format!("{variant} ({})", parts.join(", "));
            }
            _ => {}
        }
    }
    value.to_string()
}

fn outcome_field(key: &str, value: &Value) -> String {
    match value {
        Value::Number(number) => {
            if key == "winner" {
                format!("P{number}")
            } else {
                number.to_string()
            }
        }
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(ToString::to_string).collect();
            format!("[{}]", items.join(", "))
        }
        Value::String(text) => text.clone(),
        Value::Bool(value) => value.to_string(),
        Value::Null => "—".into(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn params_preserve_json_types_and_text() {
        let pairs = vec![
            "target_size=3".to_string(),
            "label=alice".to_string(),
            "flag=true".to_string(),
        ];
        let params = assemble_params(&pairs).unwrap().unwrap();
        assert_eq!(params["target_size"], Value::from(3));
        assert_eq!(params["label"], Value::from("alice"));
        assert_eq!(params["flag"], Value::from(true));
    }

    #[test]
    fn malformed_param_is_rejected() {
        assert_eq!(
            assemble_params(&["missing-separator".into()]).unwrap_err(),
            "--param must be KEY=VALUE, got 'missing-separator'"
        );
        assert_eq!(
            assemble_params(&["=value".into()]).unwrap_err(),
            "--param has an empty key: '=value'"
        );
    }

    #[test]
    fn no_params_stays_absent() {
        assert_eq!(assemble_params(&[]).unwrap(), None);
    }

    #[test]
    fn outcome_collapses_enum_and_marks_winner_as_participant() {
        let value = json!({"Win": {"winner": 1, "scores": [0, 1]}});
        assert_eq!(describe_outcome(&value), "Win (scores [0, 1], winner P1)");
    }
}
