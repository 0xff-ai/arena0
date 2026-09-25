//! Effects emitted by a program during a session event.
//!
//! Effects are deliberately not partitioned by visibility or transition kind.
//! One dispatch may mutate both state memories and emit any combination of
//! these values; the actor decides which effects need durable delivery.

use arena0_program::bounded;
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::TimerPayload;
use crate::execution::{
    MAX_EFFECT_PAYLOAD_BYTES, MAX_TERMINAL_OUTCOME_BYTES, MAX_TERMINAL_REASON_BYTES,
};

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

#[cfg(test)]
mod tests {
    use super::*;

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
