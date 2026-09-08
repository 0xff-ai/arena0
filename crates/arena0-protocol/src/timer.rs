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

impl TimerPayload {
    pub(crate) fn serialize_bounded<W: std::io::Write>(
        &self,
        writer: &mut W,
    ) -> std::io::Result<()> {
        crate::bounded::write_string(
            writer,
            &self.type_name,
            crate::execution::MAX_TERMINAL_REASON_BYTES,
            "timer type name",
        )?;
        crate::bounded::write_bytes(
            writer,
            &self.data,
            crate::execution::MAX_TIMER_PAYLOAD_BYTES,
            "timer data",
        )
    }

    pub(crate) fn deserialize_bounded<R: std::io::Read>(reader: &mut R) -> std::io::Result<Self> {
        Ok(Self {
            type_name: crate::bounded::read_string(
                reader,
                crate::execution::MAX_TERMINAL_REASON_BYTES,
                "timer type name",
            )?,
            data: crate::bounded::read_bytes(
                reader,
                crate::execution::MAX_TIMER_PAYLOAD_BYTES,
                "timer data",
            )?,
        })
    }
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
