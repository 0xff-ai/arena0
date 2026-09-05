//! Typed timer payloads shared by the SDK, sandbox, runtime, and trace tools.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

/// Program-defined typed timer value carried from `SetTimer` to timer dispatch.
#[derive(
    Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq, Hash,
)]
pub struct TimerPayload {
    /// Rust type name recorded by the SDK when the timer was scheduled.
    pub type_name: String,
    /// Borsh-encoded timer value.
    pub data: Vec<u8>,
}

/// Complete timer request emitted by a program.
#[derive(
    Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq, Hash,
)]
pub struct TimerSpec {
    /// Delay before first firing, in milliseconds.
    pub delay_ms: u64,
    /// Optional typed timer payload.
    pub payload: Option<TimerPayload>,
}

impl TimerSpec {
    /// Build an untyped timer request.
    #[must_use]
    pub fn untyped(delay_ms: u64) -> Self {
        Self {
            delay_ms,
            payload: None,
        }
    }

    /// Build a typed timer request.
    #[must_use]
    pub fn typed(delay_ms: u64, payload: TimerPayload) -> Self {
        Self {
            delay_ms,
            payload: Some(payload),
        }
    }
}
