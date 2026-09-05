//! Divergence diagnosis: comparing traces for exact replay equivalence and
//! pinpointing the first field where two entries came apart.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Participant, PublicEffect};

use super::TRACE_FORMAT_VERSION;
use super::entry::TraceEntry;

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
    /// Compared entries emitted different effect journals.
    EffectMismatch,
    /// Compared entries emitted the same effects in a different order.
    EffectOrderingMismatch,
    /// Compared entries recorded different sender witness commitments.
    WitnessMismatch,
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

impl TraceEntry {
    /// Verify that a trace entry uses the current schema version.
    pub fn validate_format(&self) -> Result<(), DivergenceDiagnostic> {
        if self.trace_version != TRACE_FORMAT_VERSION {
            return Err(DivergenceDiagnostic::new_at(
                self.step,
                DivergenceKind::TraceVersionMismatch,
                "trace_version",
                TRACE_FORMAT_VERSION,
                self.trace_version,
            ));
        }
        Ok(())
    }

    /// Verify that a trace is internally replayable as a hash chain.
    pub fn verify_chain(trace: &[Self]) -> Result<(), DivergenceDiagnostic> {
        for (idx, entry) in trace.iter().enumerate() {
            entry.validate_format()?;
            if entry.step != idx as u64 {
                return Err(DivergenceDiagnostic::new_at(
                    entry.step,
                    DivergenceKind::StepIndexMismatch,
                    "step",
                    idx,
                    entry.step,
                ));
            }
            if let Some(next) = trace.get(idx + 1)
                && entry.post_state != next.pre_state
            {
                return Err(DivergenceDiagnostic::new_at(
                    next.step,
                    DivergenceKind::ChainMismatch,
                    "pre_state",
                    entry.post_state,
                    next.pre_state,
                ));
            }
        }
        Ok(())
    }

    /// Compare two traces for exact replay equivalence.
    pub fn compare_traces(left: &[Self], right: &[Self]) -> Result<(), DivergenceDiagnostic> {
        if left.len() != right.len() {
            return Err(DivergenceDiagnostic::new_at(
                left.len().max(right.len()) as u64,
                DivergenceKind::StepCountMismatch,
                "len",
                left.len(),
                right.len(),
            ));
        }

        Self::verify_chain(left)?;
        Self::verify_chain(right)?;

        for (left, right) in left.iter().zip(right) {
            Self::compare_step(left, right)?;
        }
        Ok(())
    }

    /// Compare two records for exact replay equivalence.
    pub fn compare_step(left: &Self, right: &Self) -> Result<(), DivergenceDiagnostic> {
        left.validate_format()?;
        right.validate_format()?;
        compare_entry(left, right)
    }
}

fn compare_entry(left: &TraceEntry, right: &TraceEntry) -> Result<(), DivergenceDiagnostic> {
    if left.step != right.step {
        return Err(DivergenceDiagnostic::new_at(
            left.step,
            DivergenceKind::StepIndexMismatch,
            "step",
            left.step,
            right.step,
        ));
    }
    if left.event != right.event {
        return Err(DivergenceDiagnostic::new_at(
            left.step,
            DivergenceKind::EventMismatch,
            format!("event{}", first_json_diff_path(&left.event, &right.event)),
            &left.event,
            &right.event,
        ));
    }
    if left.pre_state != right.pre_state {
        return Err(DivergenceDiagnostic::new_at(
            left.step,
            DivergenceKind::PreStateMismatch,
            "pre_state",
            left.pre_state,
            right.pre_state,
        ));
    }
    if left.effects != right.effects {
        return Err(DivergenceDiagnostic::new_at(
            left.step,
            effect_mismatch_kind(&left.effects, &right.effects),
            format!(
                "effects{}",
                first_json_diff_path(&left.effects, &right.effects)
            ),
            &left.effects,
            &right.effects,
        ));
    }
    if left.witness != right.witness {
        return Err(DivergenceDiagnostic::new_at(
            left.step,
            DivergenceKind::WitnessMismatch,
            "witness",
            left.witness,
            right.witness,
        ));
    }
    if left.post_state != right.post_state {
        return Err(DivergenceDiagnostic::new_at(
            left.step,
            DivergenceKind::PostStateMismatch,
            "post_state",
            left.post_state,
            right.post_state,
        ));
    }
    Ok(())
}

fn effect_mismatch_kind(left: &[PublicEffect], right: &[PublicEffect]) -> DivergenceKind {
    if left.len() == right.len()
        && let (Some(mut left), Some(mut right)) =
            (sorted_json_strings(left), sorted_json_strings(right))
    {
        left.sort();
        right.sort();
        if left == right {
            return DivergenceKind::EffectOrderingMismatch;
        }
    }
    DivergenceKind::EffectMismatch
}

fn sorted_json_strings<T: Serialize>(items: &[T]) -> Option<Vec<String>> {
    items
        .iter()
        .map(|item| serde_json::to_string(item).ok())
        .collect()
}

fn first_json_diff_path<T: Serialize>(left: &T, right: &T) -> String {
    let left = serde_json::to_value(left).ok();
    let right = serde_json::to_value(right).ok();
    match (left, right) {
        (Some(left), Some(right)) => left.first_difference_path(&right),
        _ => String::new(),
    }
}

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
    use crate::trace::AggregateAttestation;
    use crate::{PublicEffect, PublicEvent, StateHash};

    fn hash(byte: u8) -> StateHash {
        StateHash([byte; 32])
    }

    fn entry(step: u64, pre: StateHash, post: StateHash) -> TraceEntry {
        TraceEntry {
            trace_version: TRACE_FORMAT_VERSION,
            step,
            event: PublicEvent::MessageReceived {
                message_id: crate::MessageId([step as u8; 32]),
                from: crate::PeerId([1; 32]),
                position: step,
                pre_state: pre,
                msg: Vec::new(),
            },
            effects: Vec::new(),
            pre_state: pre,
            post_state: post,
            fuel_used: 0,
            witness: None,
            agreement: AggregateAttestation::empty(),
        }
    }

    #[test]
    fn verify_chain_accepts_linked_steps() {
        let trace = vec![entry(0, hash(0), hash(1)), entry(1, hash(1), hash(2))];
        TraceEntry::verify_chain(&trace).unwrap();
    }

    #[test]
    fn verify_chain_reports_broken_link() {
        let trace = vec![entry(0, hash(0), hash(1)), entry(1, hash(9), hash(2))];
        let err = TraceEntry::verify_chain(&trace).unwrap_err();
        assert_eq!(err.kind, DivergenceKind::ChainMismatch);
        assert_eq!(err.step, 1);
        assert_eq!(err.field_path, "pre_state");
    }

    #[test]
    fn compare_traces_reports_effect_mismatch() {
        let left = vec![entry(0, hash(0), hash(1))];
        let mut right = left.clone();
        right[0].effects = vec![PublicEffect::SessionEnd { outcome: vec![] }];
        let err = TraceEntry::compare_traces(&left, &right).unwrap_err();
        assert_eq!(err.kind, DivergenceKind::EffectMismatch);
    }

    #[test]
    fn compare_step_reports_effect_ordering_mismatch() {
        let mut left = entry(0, hash(0), hash(1));
        left.effects = vec![
            PublicEffect::Fail {
                reason: "boom".into(),
            },
            PublicEffect::SessionAbort {
                reason: "retry".into(),
            },
        ];
        let mut right = left.clone();
        right.effects.reverse();
        let err = TraceEntry::compare_step(&left, &right).unwrap_err();
        assert_eq!(err.kind, DivergenceKind::EffectOrderingMismatch);
        assert_eq!(err.field_path, "effects[0].Fail");
    }
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
