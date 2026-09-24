//! Effects emitted by a program during a session event.
//!
//! Effects are deliberately not partitioned by visibility or transition kind.
//! One dispatch may mutate both state memories and emit any combination of
//! these values; the actor decides which effects need durable delivery.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use std::io;

use crate::TimerPayload;
use crate::bounded::{
    read_bytes as read_bounded_bytes, read_string as read_bounded_string,
    write_bytes as serialize_bounded_bytes, write_string as serialize_bounded_string,
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
    /// Arm a one-shot timer with its payload.
    SetTimer { delay_ms: u64, timer: TimerPayload },
    /// Terminate program execution immediately with an error.
    Fail { reason: String },
}

const EFFECT_SESSION_END: u8 = 0;
const EFFECT_SESSION_ABORT: u8 = 1;
const EFFECT_BROADCAST: u8 = 2;
const EFFECT_SET_TIMER: u8 = 3;
const EFFECT_FAIL: u8 = 4;

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
            Self::SetTimer { delay_ms, timer } => {
                BorshSerialize::serialize(&EFFECT_SET_TIMER, writer)?;
                BorshSerialize::serialize(delay_ms, writer)?;
                timer.serialize_bounded(writer)
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
            EFFECT_SET_TIMER => Ok(Self::SetTimer {
                delay_ms: u64::deserialize_reader(reader)?,
                timer: TimerPayload::deserialize_bounded(reader)?,
            }),
            EFFECT_FAIL => Ok(Self::Fail {
                reason: read_bounded_string(
                    reader,
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "failure reason",
                )?,
            }),
            tag => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown effect tag {tag}"),
            )),
        }
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
