//! Typed timer payloads shared by the SDK, sandbox, runtime, and trace tools.

use arena0_program::bounded;
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::execution::{MAX_TERMINAL_REASON_BYTES, MAX_TIMER_PAYLOAD_BYTES};

/// Program-defined typed timer value carried from `SetTimer` to timer dispatch.
#[derive(
    Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq, Hash,
)]
pub struct TimerPayload {
    /// Rust type name recorded by the SDK when the timer was scheduled.
    #[borsh(
        serialize_with = "bounded::write_string::<MAX_TERMINAL_REASON_BYTES>",
        deserialize_with = "bounded::read_string::<MAX_TERMINAL_REASON_BYTES>"
    )]
    pub type_name: String,
    /// Borsh-encoded timer value.
    #[borsh(
        serialize_with = "bounded::write_bytes::<MAX_TIMER_PAYLOAD_BYTES>",
        deserialize_with = "bounded::read_bytes::<MAX_TIMER_PAYLOAD_BYTES>"
    )]
    pub data: Vec<u8>,
}

impl TimerPayload {
    /// Construct the payload used by an untyped timer.
    #[must_use]
    pub fn unit() -> Self {
        Self {
            type_name: std::any::type_name::<()>().to_owned(),
            data: Vec::new(),
        }
    }
}

/// Complete timer request emitted by a program.
#[derive(
    Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq, Hash,
)]
pub struct TimerSpec {
    /// Delay before first firing, in milliseconds.
    pub delay_ms: u64,
    /// Timer payload, including the unit payload for an untyped timer.
    pub payload: TimerPayload,
}

impl TimerSpec {
    /// Build an untyped timer request.
    #[must_use]
    pub fn untyped(delay_ms: u64) -> Self {
        Self {
            delay_ms,
            payload: TimerPayload::unit(),
        }
    }

    /// Build a typed timer request.
    #[must_use]
    pub fn typed(delay_ms: u64, payload: TimerPayload) -> Self {
        Self { delay_ms, payload }
    }
}
