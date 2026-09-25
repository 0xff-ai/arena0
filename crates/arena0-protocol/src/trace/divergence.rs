//! Divergence diagnostics: the structured mismatch class and the human-readable
//! record raised when two native dispatch histories came apart.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Participant;

/// Structured class of trace replay or comparison mismatch.
#[derive(
    Serialize,
    Deserialize,
    BorshSerialize,
    BorshDeserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
)]
pub enum DivergenceKind {
    /// A trace entry used an unsupported trace format version.
    TraceVersionMismatch,
    /// A step index did not match its expected position.
    StepIndexMismatch,
    /// Adjacent records did not chain `post_state` to `pre_state`.
    ChainMismatch,
    /// Compared traces do not have the same number of entries.
    StepCountMismatch,
    /// Compared entries dispatched different events.
    EventMismatch,
    /// Compared entries started from different shared hashes.
    PreStateMismatch,
    /// Compared entries recorded different terminal effects.
    EffectMismatch,
    /// Compared entries ended with different shared hashes.
    PostStateMismatch,
    /// Compared decoded shared values differ before falling back to hashes.
    SharedMismatch,
}

/// Human-readable trace divergence diagnostic.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct DivergenceDiagnostic {
    /// Step associated with the mismatch.
    pub step: u64,
    /// Machine-readable mismatch class.
    pub kind: DivergenceKind,
    /// Local or expected value, formatted for diagnostics.
    pub left: String,
    /// Remote or actual value, formatted for diagnostics.
    pub right: String,
    /// Dot/bracket path to the first field associated with the mismatch.
    pub field_path: String,
    /// Event associated with this mismatch when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    /// Participant associated with this mismatch when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub participant: Option<Participant>,
}

impl DivergenceDiagnostic {
    /// Build a divergence diagnostic at a specific trace step and field path.
    pub fn new_at(
        step: u64,
        kind: DivergenceKind,
        field_path: impl Into<String>,
        left: impl std::fmt::Debug,
        right: impl std::fmt::Debug,
    ) -> Self {
        Self {
            step,
            kind,
            left: format!("{left:?}"),
            right: format!("{right:?}"),
            field_path: field_path.into(),
            event: None,
            participant: None,
        }
    }

    /// Attach the event that produced this divergence.
    #[must_use]
    pub fn with_event(mut self, event: impl Into<String>) -> Self {
        self.event = Some(event.into());
        self
    }

    /// Attach the participant associated with this divergence.
    #[must_use]
    pub fn with_participant(mut self, participant: Participant) -> Self {
        self.participant = Some(participant);
        self
    }
}

impl std::fmt::Display for DivergenceDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "trace divergence at step {}", self.step)?;
        if let Some(event) = &self.event {
            write!(f, " after {event}")?;
        }
        if let Some(participant) = self.participant {
            write!(f, " for {participant}")?;
        }
        write!(
            f,
            " ({:?}) at {}: left={}, right={}",
            self.kind,
            if self.field_path.is_empty() {
                "<entry>"
            } else {
                self.field_path.as_str()
            },
            short(&self.left),
            short(&self.right)
        )
    }
}

/// Cap a diagnostic value so a 32-byte hash (`Hash([1, 2, …])`) does not dump its
/// whole array into the human message; the full value stays on the struct.
fn short(v: &str) -> String {
    if v.chars().count() > 40 {
        let head: String = v.chars().take(40).collect();
        format!("{head}…")
    } else {
        v.to_string()
    }
}

impl std::error::Error for DivergenceDiagnostic {}

/// Deterministic diagnostic paths shared by trace comparison and native replay.
/// This is a presentation operation and does not affect proof semantics.
pub trait JsonDiffExt {
    /// Return the first differing path, visiting object keys in sorted order.
    /// An empty path denotes either equality or a difference at the root.
    fn first_difference_path(&self, other: &Self) -> String;
}

impl JsonDiffExt for Value {
    fn first_difference_path(&self, right: &Self) -> String {
        let left = self;
        if left == right {
            return String::new();
        }
        match (left, right) {
            (Value::Object(left), Value::Object(right)) => {
                let mut keys: Vec<_> = left.keys().chain(right.keys()).collect();
                keys.sort();
                keys.dedup();
                for key in keys {
                    match (left.get(key), right.get(key)) {
                        (Some(left), Some(right)) if left == right => {}
                        (Some(left), Some(right)) => {
                            return format!(".{key}{}", left.first_difference_path(right));
                        }
                        _ => return format!(".{key}"),
                    }
                }
                String::new()
            }
            (Value::Array(left), Value::Array(right)) => {
                for idx in 0..left.len().min(right.len()) {
                    if left[idx] != right[idx] {
                        return format!("[{idx}]{}", left[idx].first_difference_path(&right[idx]));
                    }
                }
                format!("[{}]", left.len().min(right.len()))
            }
            _ => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_difference_paths_are_deterministic() {
        for (left, right, path) in [
            (
                serde_json::json!({"z": 0, "a": [1, 2]}),
                serde_json::json!({"z": 1, "a": [1, 3]}),
                ".a[1]",
            ),
            (serde_json::json!({"a": 1}), serde_json::json!({}), ".a"),
            (serde_json::json!([1]), serde_json::json!([1, 2]), "[1]"),
            (serde_json::json!(1), serde_json::json!(2), ""),
            (serde_json::json!([1]), serde_json::json!([1]), ""),
        ] {
            assert_eq!(left.first_difference_path(&right), path);
        }
    }
}
