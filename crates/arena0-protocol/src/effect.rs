//! Effects that programs emit in response to events.
//!
//! Each [`Effect`] variant maps to exactly one host-function import. The sandbox
//! collects effects during a dispatch step and returns them to the runtime for
//! execution.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io;

use arena0_crypto::SignScheme;

use crate::TimerPayload;
use crate::bounded::{
    read_bytes as read_bounded_bytes, read_string as read_bounded_string,
    write_bytes as serialize_bounded_bytes, write_string as serialize_bounded_string,
};

/// A side effect requested by a program during a single dispatch step.
///
/// Effects are pure data; the runtime decides how (and whether) to execute them
/// after the sandbox returns. Each effect is recorded in the trace.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    // -- Lifecycle --
    /// End the current session successfully with the derived outcome.
    SessionEnd {
        /// Borsh-encoded typed `Outcome`, projected from final shared state.
        /// Visible in the trace and the receipt.
        outcome: Vec<u8>,
    },
    /// Abort the current session with a human-readable reason.
    SessionAbort {
        /// Why the session was aborted.
        reason: String,
    },

    // -- Messaging --
    /// Broadcast a binary payload to every participant. Messaging is
    /// broadcast-only: the message enters every node's public trace at the
    /// same canonical position, and the sender applies it through the same
    /// shared handler as everyone else.
    Broadcast {
        /// Borsh-encoded message payload.
        data: Vec<u8>,
    },

    // -- Callout/Input --
    /// Call out to the controlling agent (or an external actor) for input.
    Callout {
        /// Index into the program schema's callout variants.
        callout_index: u32,
        /// Borsh-encoded context for the callout prompt.
        context: Vec<u8>,
        /// Optional local pending label for trace and lifecycle diagnostics.
        pending_label: Option<String>,
        /// Optional expected output type name for typed continuation checks.
        expected_type: Option<String>,
        /// Generated continuation tag used to restore local resume state after restart.
        continuation_tag: Option<u32>,
    },

    // -- Timer --
    /// Start a one-shot timer.
    SetTimer {
        /// Delay before first firing, in milliseconds.
        delay_ms: u64,
        /// Optional typed timer payload delivered when the timer fires.
        #[serde(skip_serializing_if = "Option::is_none")]
        timer: Option<TimerPayload>,
    },

    // -- Crypto --
    /// Sign `data` with the node's key under the given scheme.
    Sign {
        /// Signing algorithm.
        scheme: SignScheme,
        /// Data to sign.
        data: Vec<u8>,
        /// Optional local pending label for trace and lifecycle diagnostics.
        pending_label: Option<String>,
        /// Expected output type name for typed continuation checks.
        expected_type: Option<String>,
        /// Generated continuation tag used to restore local resume state after restart.
        continuation_tag: Option<u32>,
    },

    // -- Control --
    /// Terminate program execution immediately with an error.
    Fail {
        /// Human-readable failure reason.
        reason: String,
    },
    /// Signal that the last callout should be re-issued due to bad input.
    ///
    /// Emitted by the generated dispatch code when `on_input` returns
    /// `InputFault::Retryable`. The runtime discards effects from this dispatch
    /// step and re-issues the previous `Callout`.
    RetryInput {
        /// Human-readable reason for the retry.
        reason: String,
    },
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
                pending_label,
                expected_type,
                continuation_tag,
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
                    pending_label.as_ref(),
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "pending label",
                )?;
                serialize_bounded_option_string(
                    writer,
                    expected_type.as_ref(),
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "expected type",
                )?;
                BorshSerialize::serialize(continuation_tag, writer)
            }
            Self::SetTimer { delay_ms, timer } => {
                BorshSerialize::serialize(&EFFECT_SET_TIMER, writer)?;
                BorshSerialize::serialize(delay_ms, writer)?;
                serialize_bounded_option_timer(writer, timer.as_ref())
            }
            Self::Sign {
                scheme,
                data,
                pending_label,
                expected_type,
                continuation_tag,
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
                    pending_label.as_ref(),
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "pending label",
                )?;
                serialize_bounded_option_string(
                    writer,
                    expected_type.as_ref(),
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "expected type",
                )?;
                BorshSerialize::serialize(continuation_tag, writer)
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
                pending_label: read_bounded_option_string(
                    reader,
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "pending label",
                )?,
                expected_type: read_bounded_option_string(
                    reader,
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "expected type",
                )?,
                continuation_tag: Option::<u32>::deserialize_reader(reader)?,
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
                pending_label: read_bounded_option_string(
                    reader,
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "pending label",
                )?,
                expected_type: read_bounded_option_string(
                    reader,
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "expected type",
                )?,
                continuation_tag: Option::<u32>::deserialize_reader(reader)?,
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

/// An effect that may be persisted in a public trace entry.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum PublicEffect {
    /// End successfully with opaque outcome bytes.
    SessionEnd { outcome: Vec<u8> },
    /// Abort with a bounded human-readable reason.
    SessionAbort { reason: String },
    /// Fail the execution with a bounded reason.
    Fail { reason: String },
}

/// An effect that may be persisted in a private trace record.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum PrivateEffect {
    /// Broadcast opaque message bytes. The private witness is committed on the
    /// resulting public message entry, so broadcasts belong to the private
    /// decision record rather than the shared trace entry.
    Broadcast { data: Vec<u8> },
    /// Request an agent callout.
    Callout {
        callout_index: u32,
        context: Vec<u8>,
        pending_label: Option<String>,
        expected_type: Option<String>,
        continuation_tag: Option<u32>,
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
        pending_label: Option<String>,
        expected_type: Option<String>,
        continuation_tag: Option<u32>,
    },
    /// Retry the current callout.
    RetryInput { reason: String },
}

/// Error returned when a raw guest effect is placed in the wrong persisted
/// trace class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectClassError {
    /// The raw value is not a public effect.
    NotPublic,
    /// The raw value is not a private effect.
    NotPrivate,
}

impl fmt::Display for EffectClassError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NotPublic => "effect is not legal in a public trace",
            Self::NotPrivate => "effect is not legal in a private trace",
        })
    }
}

impl std::error::Error for EffectClassError {}

impl TryFrom<Effect> for PublicEffect {
    type Error = EffectClassError;

    fn try_from(effect: Effect) -> Result<Self, Self::Error> {
        match effect {
            Effect::SessionEnd { outcome } => Ok(Self::SessionEnd { outcome }),
            Effect::SessionAbort { reason } => Ok(Self::SessionAbort { reason }),
            Effect::Fail { reason } => Ok(Self::Fail { reason }),
            Effect::Broadcast { .. }
            | Effect::Callout { .. }
            | Effect::SetTimer { .. }
            | Effect::Sign { .. }
            | Effect::RetryInput { .. } => Err(EffectClassError::NotPublic),
        }
    }
}

impl TryFrom<Effect> for PrivateEffect {
    type Error = EffectClassError;

    fn try_from(effect: Effect) -> Result<Self, Self::Error> {
        match effect {
            Effect::Broadcast { data } => Ok(Self::Broadcast { data }),
            Effect::Callout {
                callout_index,
                context,
                pending_label,
                expected_type,
                continuation_tag,
            } => Ok(Self::Callout {
                callout_index,
                context,
                pending_label,
                expected_type,
                continuation_tag,
            }),
            Effect::SetTimer { delay_ms, timer } => Ok(Self::SetTimer { delay_ms, timer }),
            Effect::Sign {
                scheme,
                data,
                pending_label,
                expected_type,
                continuation_tag,
            } => Ok(Self::Sign {
                scheme,
                data,
                pending_label,
                expected_type,
                continuation_tag,
            }),
            Effect::RetryInput { reason } => Ok(Self::RetryInput { reason }),
            Effect::SessionEnd { .. } | Effect::SessionAbort { .. } | Effect::Fail { .. } => {
                Err(EffectClassError::NotPrivate)
            }
        }
    }
}

impl From<PublicEffect> for Effect {
    fn from(effect: PublicEffect) -> Self {
        match effect {
            PublicEffect::SessionEnd { outcome } => Self::SessionEnd { outcome },
            PublicEffect::SessionAbort { reason } => Self::SessionAbort { reason },
            PublicEffect::Fail { reason } => Self::Fail { reason },
        }
    }
}

impl From<PrivateEffect> for Effect {
    fn from(effect: PrivateEffect) -> Self {
        match effect {
            PrivateEffect::Broadcast { data } => Self::Broadcast { data },
            PrivateEffect::Callout {
                callout_index,
                context,
                pending_label,
                expected_type,
                continuation_tag,
            } => Self::Callout {
                callout_index,
                context,
                pending_label,
                expected_type,
                continuation_tag,
            },
            PrivateEffect::SetTimer { delay_ms, timer } => Self::SetTimer { delay_ms, timer },
            PrivateEffect::Sign {
                scheme,
                data,
                pending_label,
                expected_type,
                continuation_tag,
            } => Self::Sign {
                scheme,
                data,
                pending_label,
                expected_type,
                continuation_tag,
            },
            PrivateEffect::RetryInput { reason } => Self::RetryInput { reason },
        }
    }
}

const PUBLIC_EFFECT_SESSION_END: u8 = 0;
const PUBLIC_EFFECT_SESSION_ABORT: u8 = 1;
const PUBLIC_EFFECT_FAIL: u8 = 2;

impl BorshSerialize for PublicEffect {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        match self {
            Self::SessionEnd { outcome } => {
                BorshSerialize::serialize(&PUBLIC_EFFECT_SESSION_END, writer)?;
                serialize_bounded_bytes(
                    writer,
                    outcome,
                    crate::execution::MAX_TERMINAL_OUTCOME_BYTES,
                    "terminal outcome",
                )
            }
            Self::SessionAbort { reason } => {
                BorshSerialize::serialize(&PUBLIC_EFFECT_SESSION_ABORT, writer)?;
                serialize_bounded_string(
                    writer,
                    reason,
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "terminal reason",
                )
            }
            Self::Fail { reason } => {
                BorshSerialize::serialize(&PUBLIC_EFFECT_FAIL, writer)?;
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

impl BorshDeserialize for PublicEffect {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            PUBLIC_EFFECT_SESSION_END => Ok(Self::SessionEnd {
                outcome: read_bounded_bytes(
                    reader,
                    crate::execution::MAX_TERMINAL_OUTCOME_BYTES,
                    "terminal outcome",
                )?,
            }),
            PUBLIC_EFFECT_SESSION_ABORT => Ok(Self::SessionAbort {
                reason: read_bounded_string(
                    reader,
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "terminal reason",
                )?,
            }),
            PUBLIC_EFFECT_FAIL => Ok(Self::Fail {
                reason: read_bounded_string(
                    reader,
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "failure reason",
                )?,
            }),
            tag => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown public effect tag {tag}"),
            )),
        }
    }
}

const PRIVATE_EFFECT_CALLOUT: u8 = 0;
const PRIVATE_EFFECT_SET_TIMER: u8 = 1;
const PRIVATE_EFFECT_SIGN: u8 = 2;
const PRIVATE_EFFECT_RETRY_INPUT: u8 = 3;
const PRIVATE_EFFECT_BROADCAST: u8 = 4;

impl BorshSerialize for PrivateEffect {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        match self {
            Self::Broadcast { data } => {
                BorshSerialize::serialize(&PRIVATE_EFFECT_BROADCAST, writer)?;
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
                pending_label,
                expected_type,
                continuation_tag,
            } => {
                BorshSerialize::serialize(&PRIVATE_EFFECT_CALLOUT, writer)?;
                BorshSerialize::serialize(callout_index, writer)?;
                serialize_bounded_bytes(
                    writer,
                    context,
                    crate::execution::MAX_EFFECT_PAYLOAD_BYTES,
                    "callout context",
                )?;
                serialize_bounded_option_string(
                    writer,
                    pending_label.as_ref(),
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "pending label",
                )?;
                serialize_bounded_option_string(
                    writer,
                    expected_type.as_ref(),
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "expected type",
                )?;
                BorshSerialize::serialize(continuation_tag, writer)
            }
            Self::SetTimer { delay_ms, timer } => {
                BorshSerialize::serialize(&PRIVATE_EFFECT_SET_TIMER, writer)?;
                BorshSerialize::serialize(delay_ms, writer)?;
                serialize_bounded_option_timer(writer, timer.as_ref())
            }
            Self::Sign {
                scheme,
                data,
                pending_label,
                expected_type,
                continuation_tag,
            } => {
                BorshSerialize::serialize(&PRIVATE_EFFECT_SIGN, writer)?;
                BorshSerialize::serialize(scheme, writer)?;
                serialize_bounded_bytes(
                    writer,
                    data,
                    crate::execution::MAX_EFFECT_PAYLOAD_BYTES,
                    "signature payload",
                )?;
                serialize_bounded_option_string(
                    writer,
                    pending_label.as_ref(),
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "pending label",
                )?;
                serialize_bounded_option_string(
                    writer,
                    expected_type.as_ref(),
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "expected type",
                )?;
                BorshSerialize::serialize(continuation_tag, writer)
            }
            Self::RetryInput { reason } => {
                BorshSerialize::serialize(&PRIVATE_EFFECT_RETRY_INPUT, writer)?;
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

impl BorshDeserialize for PrivateEffect {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            PRIVATE_EFFECT_BROADCAST => Ok(Self::Broadcast {
                data: read_bounded_bytes(
                    reader,
                    crate::execution::MAX_EFFECT_PAYLOAD_BYTES,
                    "broadcast payload",
                )?,
            }),
            PRIVATE_EFFECT_CALLOUT => Ok(Self::Callout {
                callout_index: u32::deserialize_reader(reader)?,
                context: read_bounded_bytes(
                    reader,
                    crate::execution::MAX_EFFECT_PAYLOAD_BYTES,
                    "callout context",
                )?,
                pending_label: read_bounded_option_string(
                    reader,
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "pending label",
                )?,
                expected_type: read_bounded_option_string(
                    reader,
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "expected type",
                )?,
                continuation_tag: Option::<u32>::deserialize_reader(reader)?,
            }),
            PRIVATE_EFFECT_SET_TIMER => Ok(Self::SetTimer {
                delay_ms: u64::deserialize_reader(reader)?,
                timer: read_bounded_option_timer(reader)?,
            }),
            PRIVATE_EFFECT_SIGN => Ok(Self::Sign {
                scheme: SignScheme::deserialize_reader(reader)?,
                data: read_bounded_bytes(
                    reader,
                    crate::execution::MAX_EFFECT_PAYLOAD_BYTES,
                    "signature payload",
                )?,
                pending_label: read_bounded_option_string(
                    reader,
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "pending label",
                )?,
                expected_type: read_bounded_option_string(
                    reader,
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "expected type",
                )?,
                continuation_tag: Option::<u32>::deserialize_reader(reader)?,
            }),
            PRIVATE_EFFECT_RETRY_INPUT => Ok(Self::RetryInput {
                reason: read_bounded_string(
                    reader,
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "retry reason",
                )?,
            }),
            tag => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown private effect tag {tag}"),
            )),
        }
    }
}

fn serialize_bounded_option_string<W: borsh::io::Write>(
    writer: &mut W,
    value: Option<&String>,
    max: usize,
    field: &'static str,
) -> io::Result<()> {
    match value {
        None => BorshSerialize::serialize(&0u8, writer),
        Some(value) => {
            BorshSerialize::serialize(&1u8, writer)?;
            serialize_bounded_string(writer, value, max, field)
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
            serialize_bounded_string(
                writer,
                &timer.type_name,
                crate::execution::MAX_TERMINAL_REASON_BYTES,
                "timer type name",
            )?;
            serialize_bounded_bytes(
                writer,
                &timer.data,
                crate::execution::MAX_TIMER_PAYLOAD_BYTES,
                "timer data",
            )
        }
    }
}

fn read_bounded_option_string<R: borsh::io::Read>(
    reader: &mut R,
    max: usize,
    field: &'static str,
) -> io::Result<Option<String>> {
    match u8::deserialize_reader(reader)? {
        0 => Ok(None),
        1 => read_bounded_string(reader, max, field).map(Some),
        tag => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown optional string tag {tag}"),
        )),
    }
}

fn read_bounded_option_timer<R: borsh::io::Read>(
    reader: &mut R,
) -> io::Result<Option<TimerPayload>> {
    match u8::deserialize_reader(reader)? {
        0 => Ok(None),
        1 => Ok(Some(TimerPayload {
            type_name: read_bounded_string(
                reader,
                crate::execution::MAX_TERMINAL_REASON_BYTES,
                "timer type name",
            )?,
            data: read_bounded_bytes(
                reader,
                crate::execution::MAX_TIMER_PAYLOAD_BYTES,
                "timer data",
            )?,
        })),
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
    /// Verbose debugging information.
    Debug,
    /// Normal operational messages.
    Info,
    /// Potential issues worth attention.
    Warn,
    /// Errors that may affect correctness.
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
    /// Clean shutdown initiated by either side.
    Normal,
    /// No response within the configured timeout.
    Timeout,
    /// Wire protocol violation.
    ProtocolError,
    /// Underlying transport connection dropped.
    ConnectionLost,
    /// Peer was explicitly removed by the program or operator.
    Kicked,
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0_crypto::HashAlgorithm;

    #[test]
    fn borsh_round_trip_all_effect_variants() {
        let variants: Vec<Effect> = vec![
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
                pending_label: Some("thinking".into()),
                expected_type: Some("Move".into()),
                continuation_tag: None,
            },
            Effect::SetTimer {
                delay_ms: 1000,
                timer: None,
            },
            Effect::Sign {
                scheme: SignScheme::Ed25519,
                data: vec![30, 40],
                pending_label: Some("signing".into()),
                expected_type: Some("Signature".into()),
                continuation_tag: None,
            },
            Effect::Fail {
                reason: "something went wrong".into(),
            },
            Effect::RetryInput {
                reason: "bad input".into(),
            },
        ];

        for variant in &variants {
            let encoded = borsh::to_vec(variant).expect("serialize");
            let decoded: Effect = borsh::from_slice(&encoded).expect("deserialize");
            assert_eq!(*variant, decoded);
        }
    }

    #[test]
    fn borsh_round_trip_sign_schemes_and_hash_algorithms() {
        for variant in [SignScheme::Ed25519, SignScheme::Bls] {
            let encoded = borsh::to_vec(&variant).expect("serialize");
            assert_eq!(borsh::from_slice::<SignScheme>(&encoded).unwrap(), variant);
        }
        for variant in [
            HashAlgorithm::Blake3,
            HashAlgorithm::Sha256,
            HashAlgorithm::Keccak256,
        ] {
            let encoded = borsh::to_vec(&variant).expect("serialize");
            assert_eq!(
                borsh::from_slice::<HashAlgorithm>(&encoded).unwrap(),
                variant
            );
        }
    }

    #[test]
    fn persisted_effect_tags_are_fixed_and_unknown_tags_are_rejected() {
        let raw = Effect::Fail {
            reason: String::new(),
        };
        let public = PublicEffect::Fail {
            reason: String::new(),
        };
        let private = PrivateEffect::Broadcast { data: Vec::new() };
        assert_eq!(borsh::to_vec(&raw).unwrap()[0], EFFECT_FAIL);
        assert_eq!(borsh::to_vec(&public).unwrap()[0], PUBLIC_EFFECT_FAIL);
        assert_eq!(
            borsh::to_vec(&private).unwrap()[0],
            PRIVATE_EFFECT_BROADCAST
        );
        assert!(borsh::from_slice::<Effect>(&[0xff]).is_err());
        assert!(borsh::from_slice::<PublicEffect>(&[0xff]).is_err());
        assert!(borsh::from_slice::<PrivateEffect>(&[0xff]).is_err());
    }
}
