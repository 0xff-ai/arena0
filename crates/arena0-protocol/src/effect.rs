//! Effects emitted by a program during a session event.
//!
//! Effects are deliberately not partitioned by visibility or transition kind.
//! One dispatch may mutate both state memories and emit any combination of
//! these values; the actor decides which effects need durable delivery.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use std::io;

use arena0_crypto::SignScheme;

use crate::TimerPayload;
use crate::bounded::{
    read_bytes as read_bounded_bytes, read_option_string as read_bounded_option_string,
    read_string as read_bounded_string, write_bytes as serialize_bounded_bytes,
    write_option_string as serialize_bounded_option_string,
    write_string as serialize_bounded_string,
};

/// A side effect requested by a program during one event dispatch.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// End the current session successfully with opaque outcome bytes.
    SessionEnd { outcome: Vec<u8> },
    /// Abort the current session with a human-readable reason.
    SessionAbort { reason: String },
    /// Broadcast an opaque program message to the session participants.
    Broadcast { data: Vec<u8> },
    /// Call out to the controlling agent or an external actor for input.
    Callout {
        callout_index: u32,
        context: Vec<u8>,
        expected_type: Option<String>,
    },
    /// Arm a one-shot timer.
    SetTimer {
        delay_ms: u64,
        timer: Option<TimerPayload>,
    },
    /// Request a host signature.
    Sign {
        scheme: SignScheme,
        data: Vec<u8>,
        expected_type: Option<String>,
    },
    /// Terminate program execution immediately with an error.
    Fail { reason: String },
    /// Re-issue the pending callout after a retryable input fault.
    RetryInput { reason: String },
}

const EFFECT_SESSION_END: u8 = 0;
const EFFECT_SESSION_ABORT: u8 = 1;
const EFFECT_BROADCAST: u8 = 2;
const EFFECT_CALLOUT: u8 = 3;
const EFFECT_SET_TIMER: u8 = 4;
const EFFECT_SIGN: u8 = 5;
const EFFECT_FAIL: u8 = 6;
const EFFECT_RETRY_INPUT: u8 = 7;

impl BorshSerialize for Effect {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        match self {
            Self::SessionEnd { outcome } => {
                BorshSerialize::serialize(&EFFECT_SESSION_END, writer)?;
                serialize_bounded_bytes(
                    writer,
                    outcome,
                    crate::execution::MAX_TERMINAL_OUTCOME_BYTES,
                    "terminal outcome",
                )
            }
            Self::SessionAbort { reason } => {
                BorshSerialize::serialize(&EFFECT_SESSION_ABORT, writer)?;
                serialize_bounded_string(
                    writer,
                    reason,
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "terminal reason",
                )
            }
            Self::Broadcast { data } => {
                BorshSerialize::serialize(&EFFECT_BROADCAST, writer)?;
                serialize_bounded_bytes(
                    writer,
                    data,
                    crate::execution::MAX_EFFECT_PAYLOAD_BYTES,
                    "broadcast payload",
                )
            }
            Self::Callout {
                callout_index,
                context,
                expected_type,
            } => {
                BorshSerialize::serialize(&EFFECT_CALLOUT, writer)?;
                BorshSerialize::serialize(callout_index, writer)?;
                serialize_bounded_bytes(
                    writer,
                    context,
                    crate::execution::MAX_EFFECT_PAYLOAD_BYTES,
                    "callout context",
                )?;
                serialize_bounded_option_string(
                    writer,
                    expected_type.as_deref(),
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "expected type",
                )
            }
            Self::SetTimer { delay_ms, timer } => {
                BorshSerialize::serialize(&EFFECT_SET_TIMER, writer)?;
                BorshSerialize::serialize(delay_ms, writer)?;
                serialize_bounded_option_timer(writer, timer.as_ref())
            }
            Self::Sign {
                scheme,
                data,
                expected_type,
            } => {
                BorshSerialize::serialize(&EFFECT_SIGN, writer)?;
                BorshSerialize::serialize(scheme, writer)?;
                serialize_bounded_bytes(
                    writer,
                    data,
                    crate::execution::MAX_EFFECT_PAYLOAD_BYTES,
                    "signature payload",
                )?;
                serialize_bounded_option_string(
                    writer,
                    expected_type.as_deref(),
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "expected type",
                )
            }
            Self::Fail { reason } => {
                BorshSerialize::serialize(&EFFECT_FAIL, writer)?;
                serialize_bounded_string(
                    writer,
                    reason,
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "failure reason",
                )
            }
            Self::RetryInput { reason } => {
                BorshSerialize::serialize(&EFFECT_RETRY_INPUT, writer)?;
                serialize_bounded_string(
                    writer,
                    reason,
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "retry reason",
                )
            }
        }
    }
}

impl BorshDeserialize for Effect {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            EFFECT_SESSION_END => Ok(Self::SessionEnd {
                outcome: read_bounded_bytes(
                    reader,
                    crate::execution::MAX_TERMINAL_OUTCOME_BYTES,
                    "terminal outcome",
                )?,
            }),
            EFFECT_SESSION_ABORT => Ok(Self::SessionAbort {
                reason: read_bounded_string(
                    reader,
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "terminal reason",
                )?,
            }),
            EFFECT_BROADCAST => Ok(Self::Broadcast {
                data: read_bounded_bytes(
                    reader,
                    crate::execution::MAX_EFFECT_PAYLOAD_BYTES,
                    "broadcast payload",
                )?,
            }),
            EFFECT_CALLOUT => Ok(Self::Callout {
                callout_index: u32::deserialize_reader(reader)?,
                context: read_bounded_bytes(
                    reader,
                    crate::execution::MAX_EFFECT_PAYLOAD_BYTES,
                    "callout context",
                )?,
                expected_type: read_bounded_option_string(
                    reader,
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "expected type",
                )?,
            }),
            EFFECT_SET_TIMER => Ok(Self::SetTimer {
                delay_ms: u64::deserialize_reader(reader)?,
                timer: read_bounded_option_timer(reader)?,
            }),
            EFFECT_SIGN => Ok(Self::Sign {
                scheme: SignScheme::deserialize_reader(reader)?,
                data: read_bounded_bytes(
                    reader,
                    crate::execution::MAX_EFFECT_PAYLOAD_BYTES,
                    "signature payload",
                )?,
                expected_type: read_bounded_option_string(
                    reader,
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "expected type",
                )?,
            }),
            EFFECT_FAIL => Ok(Self::Fail {
                reason: read_bounded_string(
                    reader,
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "failure reason",
                )?,
            }),
            EFFECT_RETRY_INPUT => Ok(Self::RetryInput {
                reason: read_bounded_string(
                    reader,
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "retry reason",
                )?,
            }),
            tag => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown effect tag {tag}"),
            )),
        }
    }
}

fn serialize_bounded_option_timer<W: borsh::io::Write>(
    writer: &mut W,
    timer: Option<&TimerPayload>,
) -> io::Result<()> {
    match timer {
        None => BorshSerialize::serialize(&0u8, writer),
        Some(timer) => {
            BorshSerialize::serialize(&1u8, writer)?;
            timer.serialize_bounded(writer)
        }
    }
}

fn read_bounded_option_timer<R: borsh::io::Read>(
    reader: &mut R,
) -> io::Result<Option<TimerPayload>> {
    match u8::deserialize_reader(reader)? {
        0 => Ok(None),
        1 => TimerPayload::deserialize_bounded(reader).map(Some),
        tag => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown optional timer tag {tag}"),
        )),
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

/// Why a peer was disconnected from a session.
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
pub enum DisconnectReason {
    Normal,
    Timeout,
    ProtocolError,
    ConnectionLost,
    Kicked,
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
            Effect::Callout {
                callout_index: 0,
                context: vec![1, 2, 3],
                expected_type: Some("Move".into()),
            },
            Effect::SetTimer {
                delay_ms: 1000,
                timer: None,
            },
            Effect::Sign {
                scheme: SignScheme::Ed25519,
                data: vec![30, 40],
                expected_type: Some("Signature".into()),
            },
            Effect::Fail {
                reason: "failed".into(),
            },
            Effect::RetryInput {
                reason: "bad input".into(),
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

    #[test]
    fn callout_optional_strings_are_bounded() {
        let effect = Effect::Callout {
            callout_index: 0,
            context: Vec::new(),
            expected_type: None,
        };
        let encoded = borsh::to_vec(&effect).expect("serialize");
        assert_eq!(Effect::try_from_slice(&encoded).expect("decode"), effect);
        let oversized = Effect::Callout {
            callout_index: 0,
            context: Vec::new(),
            expected_type: Some("x".repeat(crate::execution::MAX_TERMINAL_REASON_BYTES + 1)),
        };
        assert!(borsh::to_vec(&oversized).is_err());
    }
}
