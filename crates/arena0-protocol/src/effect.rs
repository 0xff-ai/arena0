//! Effects emitted by a program during a session event.
//!
//! Effects are deliberately not partitioned by visibility or transition kind.
//! One dispatch may mutate both state memories and emit any combination of
//! these values; the actor decides which effects need durable delivery.

use arena0_program::abi::log_level;
use arena0_program::bounded;
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::TimerPayload;
use crate::execution::{
    MAX_EFFECT_PAYLOAD_BYTES, MAX_TERMINAL_OUTCOME_BYTES, MAX_TERMINAL_REASON_BYTES,
};

/// Kind of an [`Effect`], for diagnostics that never expose its payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectKind {
    SessionEnd,
    SessionAbort,
    Broadcast,
    SetTimer,
    Fail,
}

/// Kind and bounded payload size of one effect. The payload itself is
/// deliberately never returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectSummary {
    pub kind: EffectKind,
    pub payload_bytes: Option<u64>,
}

/// A side effect requested by a program during one event dispatch.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// End the current session successfully with opaque outcome bytes.
    SessionEnd {
        #[borsh(
            serialize_with = "bounded::write_bytes::<MAX_TERMINAL_OUTCOME_BYTES>",
            deserialize_with = "bounded::read_bytes::<MAX_TERMINAL_OUTCOME_BYTES>"
        )]
        outcome: Vec<u8>,
    },
    /// Abort the current session with a human-readable reason.
    SessionAbort {
        #[borsh(
            serialize_with = "bounded::write_string::<MAX_TERMINAL_REASON_BYTES>",
            deserialize_with = "bounded::read_string::<MAX_TERMINAL_REASON_BYTES>"
        )]
        reason: String,
    },
    /// Broadcast an opaque program message to the session participants.
    Broadcast {
        #[borsh(
            serialize_with = "bounded::write_bytes::<MAX_EFFECT_PAYLOAD_BYTES>",
            deserialize_with = "bounded::read_bytes::<MAX_EFFECT_PAYLOAD_BYTES>"
        )]
        data: Vec<u8>,
    },
    /// Arm a one-shot timer with its payload.
    SetTimer { delay_ms: u64, timer: TimerPayload },
    /// Terminate program execution immediately with an error.
    Fail {
        #[borsh(
            serialize_with = "bounded::write_string::<MAX_TERMINAL_REASON_BYTES>",
            deserialize_with = "bounded::read_string::<MAX_TERMINAL_REASON_BYTES>"
        )]
        reason: String,
    },
}

impl Effect {
    /// Whether this effect ends the session: `SessionEnd`, `SessionAbort`,
    /// or `Fail`. A dispatch may emit at most one.
    #[must_use]
    pub const fn is_lifecycle(&self) -> bool {
        matches!(
            self,
            Self::SessionEnd { .. } | Self::SessionAbort { .. } | Self::Fail { .. }
        )
    }
}

/// Severity level for program log output.
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
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    /// The level's tag in the `log` host import.
    #[must_use]
    pub const fn abi_tag(self) -> u32 {
        match self {
            Self::Debug => log_level::DEBUG,
            Self::Info => log_level::INFO,
            Self::Warn => log_level::WARN,
            Self::Error => log_level::ERROR,
        }
    }

    /// The level a `log` host-import tag names, if any.
    #[must_use]
    pub const fn from_abi_tag(tag: u32) -> Option<Self> {
        match tag {
            log_level::DEBUG => Some(Self::Debug),
            log_level::INFO => Some(Self::Info),
            log_level::WARN => Some(Self::Warn),
            log_level::ERROR => Some(Self::Error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_level_tags_round_trip() {
        for level in [
            LogLevel::Debug,
            LogLevel::Info,
            LogLevel::Warn,
            LogLevel::Error,
        ] {
            assert_eq!(LogLevel::from_abi_tag(level.abi_tag()), Some(level));
        }
        assert_eq!(LogLevel::from_abi_tag(4), None);
    }

    #[test]
    fn borsh_round_trip_all_effect_variants() {
        let variants = vec![
            Effect::SessionEnd {
                outcome: vec![0xFF],
            },
            Effect::SessionAbort {
                reason: "aborted".into(),
            },
            Effect::Broadcast {
                data: vec![1, 2, 3],
            },
            Effect::SetTimer {
                delay_ms: 1000,
                timer: TimerPayload::unit(),
            },
            Effect::Fail {
                reason: "failed".into(),
            },
        ];

        for effect in variants {
            let encoded = borsh::to_vec(&effect).expect("serialize");
            assert_eq!(
                borsh::from_slice::<Effect>(&encoded).expect("deserialize"),
                effect
            );
        }
    }

    #[test]
    fn unknown_effect_tags_are_rejected() {
        assert!(borsh::from_slice::<Effect>(&[0xff]).is_err());
    }
}
