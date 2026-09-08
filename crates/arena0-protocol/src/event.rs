//! Inbound events dispatched to programs by the runtime.
//!
//! Each [`Event`] variant represents something that happened outside the
//! program: a session boundary, a message arrival, or a timer firing. The
//! program responds by returning zero or more [`Effect`](crate::Effect) values.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io;

use crate::bounded::{read_bytes as read_bounded_bytes, write_bytes as serialize_bounded_bytes};
use crate::{Ensemble, MessageId, PeerId, StateHash, TimerPayload};

/// An event dispatched to a program during a single execution step.
///
/// The type parameter `M` controls the message payload type. The runtime uses
/// `Event<Vec<u8>>` (raw bytes); the SDK decodes to `Event<M>` where `M` is the
/// program's typed message enum.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Event<M = Vec<u8>> {
    /// A session has been established with a committed participant ensemble.
    /// A shared boundary event: applied by every node at public position 0,
    /// byte-identical (it carries only the committed ensemble).
    SessionStarted {
        /// Committed participant set in canonical order.
        ensemble: Ensemble,
    },
    /// A broadcast message applied at a canonical public position. A shared
    /// event: every node, including the sender, runs the same handler at the
    /// same position. `message_id` is the envelope's content address,
    /// `position` the global trace position the message was computed against, and
    /// `pre_state` the state hash at that position.
    MessageReceived {
        message_id: MessageId,
        from: PeerId,
        position: u64,
        pre_state: StateHash,
        msg: M,
    },
    /// The controlling agent submitted input in response to a [`Callout`](crate::Effect::Callout).
    InputReceived {
        callout_index: u32,
        data: Vec<u8>,
        continuation_tag: Option<u32>,
    },
    /// A previously set timer fired.
    TimerFired,
    /// A previously set typed timer fired.
    TypedTimerFired { timer: TimerPayload },
    /// Completed [`Sign`](crate::Effect::Sign) effect.
    Signed {
        signature: Vec<u8>,
        continuation_tag: Option<u32>,
    },
    /// Run the program's local decision code after a public entry applied.
    /// A local event: the handler sees a read-only shared view, may write
    /// private state, draw entropy, request callouts, and emit broadcasts.
    /// Recorded only in the node's private trace section.
    React,
}

impl Event<Vec<u8>> {
    /// Decode the message payload from Borsh, converting `Event<Vec<u8>>` to `Event<M>`.
    ///
    /// Non-message variants pass through unchanged. Returns `Err` only when a
    /// `MessageReceived` payload fails to deserialize.
    pub fn decode<M: BorshDeserialize>(self) -> Result<Event<M>, std::io::Error> {
        Ok(match self {
            Self::MessageReceived {
                message_id,
                from,
                position,
                pre_state,
                msg,
            } => Event::MessageReceived {
                message_id,
                from,
                position,
                pre_state,
                msg: borsh::from_slice(&msg)?,
            },
            Self::SessionStarted { ensemble } => Event::SessionStarted { ensemble },
            Self::InputReceived {
                callout_index,
                data,
                continuation_tag,
            } => Event::InputReceived {
                callout_index,
                data,
                continuation_tag,
            },
            Self::TimerFired => Event::TimerFired,
            Self::TypedTimerFired { timer } => Event::TypedTimerFired { timer },
            Self::Signed {
                signature,
                continuation_tag,
            } => Event::Signed {
                signature,
                continuation_tag,
            },
            Self::React => Event::React,
        })
    }
}

const EVENT_SESSION_STARTED: u8 = 0;
const EVENT_MESSAGE_RECEIVED: u8 = 1;
const EVENT_INPUT_RECEIVED: u8 = 2;
const EVENT_TIMER_FIRED: u8 = 3;
const EVENT_TYPED_TIMER_FIRED: u8 = 4;
const EVENT_SIGNED: u8 = 5;
const EVENT_REACT: u8 = 6;

impl<M: BorshSerialize> BorshSerialize for Event<M> {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        match self {
            Self::SessionStarted { ensemble } => {
                BorshSerialize::serialize(&EVENT_SESSION_STARTED, writer)?;
                BorshSerialize::serialize(ensemble, writer)
            }
            Self::MessageReceived {
                message_id,
                from,
                position,
                pre_state,
                msg,
            } => {
                BorshSerialize::serialize(&EVENT_MESSAGE_RECEIVED, writer)?;
                BorshSerialize::serialize(message_id, writer)?;
                BorshSerialize::serialize(from, writer)?;
                BorshSerialize::serialize(position, writer)?;
                BorshSerialize::serialize(pre_state, writer)?;
                BorshSerialize::serialize(msg, writer)
            }
            Self::InputReceived {
                callout_index,
                data,
                continuation_tag,
            } => {
                BorshSerialize::serialize(&EVENT_INPUT_RECEIVED, writer)?;
                BorshSerialize::serialize(callout_index, writer)?;
                serialize_bounded_bytes(
                    writer,
                    data,
                    crate::execution::MAX_EFFECT_PAYLOAD_BYTES,
                    "event input payload",
                )?;
                BorshSerialize::serialize(continuation_tag, writer)
            }
            Self::TimerFired => BorshSerialize::serialize(&EVENT_TIMER_FIRED, writer),
            Self::TypedTimerFired { timer } => {
                BorshSerialize::serialize(&EVENT_TYPED_TIMER_FIRED, writer)?;
                timer.serialize_bounded(writer)
            }
            Self::Signed {
                signature,
                continuation_tag,
            } => {
                BorshSerialize::serialize(&EVENT_SIGNED, writer)?;
                serialize_bounded_bytes(
                    writer,
                    signature,
                    crate::execution::MAX_EFFECT_PAYLOAD_BYTES,
                    "event signature payload",
                )?;
                BorshSerialize::serialize(continuation_tag, writer)
            }
            Self::React => BorshSerialize::serialize(&EVENT_REACT, writer),
        }
    }
}

impl<M: BorshDeserialize> BorshDeserialize for Event<M> {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            EVENT_SESSION_STARTED => Ok(Self::SessionStarted {
                ensemble: borsh::BorshDeserialize::deserialize_reader(reader)?,
            }),
            EVENT_MESSAGE_RECEIVED => Ok(Self::MessageReceived {
                message_id: borsh::BorshDeserialize::deserialize_reader(reader)?,
                from: borsh::BorshDeserialize::deserialize_reader(reader)?,
                position: borsh::BorshDeserialize::deserialize_reader(reader)?,
                pre_state: borsh::BorshDeserialize::deserialize_reader(reader)?,
                msg: M::deserialize_reader(reader)?,
            }),
            EVENT_INPUT_RECEIVED => Ok(Self::InputReceived {
                callout_index: borsh::BorshDeserialize::deserialize_reader(reader)?,
                data: read_bounded_bytes(
                    reader,
                    crate::execution::MAX_EFFECT_PAYLOAD_BYTES,
                    "event input payload",
                )?,
                continuation_tag: borsh::BorshDeserialize::deserialize_reader(reader)?,
            }),
            EVENT_TIMER_FIRED => Ok(Self::TimerFired),
            EVENT_TYPED_TIMER_FIRED => Ok(Self::TypedTimerFired {
                timer: TimerPayload::deserialize_bounded(reader)?,
            }),
            EVENT_SIGNED => Ok(Self::Signed {
                signature: read_bounded_bytes(
                    reader,
                    crate::execution::MAX_EFFECT_PAYLOAD_BYTES,
                    "event signature payload",
                )?,
                continuation_tag: borsh::BorshDeserialize::deserialize_reader(reader)?,
            }),
            EVENT_REACT => Ok(Self::React),
            tag => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown event tag {tag}"),
            )),
        }
    }
}

/// A shared event that is legal in a public trace entry.  Local answers,
/// timers, signatures, and reactions cannot be represented by this type.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum PublicEvent {
    /// The committed session start boundary.
    SessionStarted { ensemble: Ensemble },
    /// A broadcast message applied at a public position.
    MessageReceived {
        message_id: MessageId,
        from: PeerId,
        position: u64,
        pre_state: StateHash,
        msg: Vec<u8>,
    },
}

/// A local event that is legal in a private trace record.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum PrivateEvent {
    /// A callout answer.
    InputReceived {
        callout_index: u32,
        data: Vec<u8>,
        continuation_tag: Option<u32>,
    },
    /// An untyped one-shot timer firing.
    TimerFired,
    /// A typed one-shot timer firing.
    TypedTimerFired { timer: TimerPayload },
    /// A completed host signature.
    Signed {
        signature: Vec<u8>,
        continuation_tag: Option<u32>,
    },
    /// A local reaction after a public event.
    React,
}

/// Error returned when a raw guest event is placed in the wrong persisted
/// trace class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventClassError {
    /// The raw value is not a public event.
    NotPublic,
    /// The raw value is not a private event.
    NotPrivate,
}

impl fmt::Display for EventClassError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NotPublic => "event is not legal in a public trace",
            Self::NotPrivate => "event is not legal in a private trace",
        })
    }
}

impl std::error::Error for EventClassError {}

impl TryFrom<Event<Vec<u8>>> for PublicEvent {
    type Error = EventClassError;

    fn try_from(event: Event<Vec<u8>>) -> Result<Self, Self::Error> {
        match event {
            Event::SessionStarted { ensemble } => Ok(Self::SessionStarted { ensemble }),
            Event::MessageReceived {
                message_id,
                from,
                position,
                pre_state,
                msg,
            } => Ok(Self::MessageReceived {
                message_id,
                from,
                position,
                pre_state,
                msg,
            }),
            Event::InputReceived { .. }
            | Event::TimerFired
            | Event::TypedTimerFired { .. }
            | Event::Signed { .. }
            | Event::React => Err(EventClassError::NotPublic),
        }
    }
}

impl TryFrom<Event<Vec<u8>>> for PrivateEvent {
    type Error = EventClassError;

    fn try_from(event: Event<Vec<u8>>) -> Result<Self, Self::Error> {
        match event {
            Event::InputReceived {
                callout_index,
                data,
                continuation_tag,
            } => Ok(Self::InputReceived {
                callout_index,
                data,
                continuation_tag,
            }),
            Event::TimerFired => Ok(Self::TimerFired),
            Event::TypedTimerFired { timer } => Ok(Self::TypedTimerFired { timer }),
            Event::Signed {
                signature,
                continuation_tag,
            } => Ok(Self::Signed {
                signature,
                continuation_tag,
            }),
            Event::React => Ok(Self::React),
            Event::SessionStarted { .. } | Event::MessageReceived { .. } => {
                Err(EventClassError::NotPrivate)
            }
        }
    }
}

impl From<PublicEvent> for Event<Vec<u8>> {
    fn from(event: PublicEvent) -> Self {
        match event {
            PublicEvent::SessionStarted { ensemble } => Self::SessionStarted { ensemble },
            PublicEvent::MessageReceived {
                message_id,
                from,
                position,
                pre_state,
                msg,
            } => Self::MessageReceived {
                message_id,
                from,
                position,
                pre_state,
                msg,
            },
        }
    }
}

impl From<PrivateEvent> for Event<Vec<u8>> {
    fn from(event: PrivateEvent) -> Self {
        match event {
            PrivateEvent::InputReceived {
                callout_index,
                data,
                continuation_tag,
            } => Self::InputReceived {
                callout_index,
                data,
                continuation_tag,
            },
            PrivateEvent::TimerFired => Self::TimerFired,
            PrivateEvent::TypedTimerFired { timer } => Self::TypedTimerFired { timer },
            PrivateEvent::Signed {
                signature,
                continuation_tag,
            } => Self::Signed {
                signature,
                continuation_tag,
            },
            PrivateEvent::React => Self::React,
        }
    }
}

const PUBLIC_EVENT_SESSION_STARTED: u8 = 0;
const PUBLIC_EVENT_MESSAGE_RECEIVED: u8 = 1;

impl BorshSerialize for PublicEvent {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        match self {
            Self::SessionStarted { ensemble } => {
                BorshSerialize::serialize(&PUBLIC_EVENT_SESSION_STARTED, writer)?;
                BorshSerialize::serialize(ensemble, writer)
            }
            Self::MessageReceived {
                message_id,
                from,
                position,
                pre_state,
                msg,
            } => {
                BorshSerialize::serialize(&PUBLIC_EVENT_MESSAGE_RECEIVED, writer)?;
                BorshSerialize::serialize(message_id, writer)?;
                BorshSerialize::serialize(from, writer)?;
                BorshSerialize::serialize(position, writer)?;
                BorshSerialize::serialize(pre_state, writer)?;
                serialize_bounded_bytes(
                    writer,
                    msg,
                    crate::execution::MAX_EFFECT_PAYLOAD_BYTES,
                    "public event payload",
                )
            }
        }
    }
}

impl BorshDeserialize for PublicEvent {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            PUBLIC_EVENT_SESSION_STARTED => Ok(Self::SessionStarted {
                ensemble: Ensemble::deserialize_reader(reader)?,
            }),
            PUBLIC_EVENT_MESSAGE_RECEIVED => Ok(Self::MessageReceived {
                message_id: MessageId::deserialize_reader(reader)?,
                from: PeerId::deserialize_reader(reader)?,
                position: u64::deserialize_reader(reader)?,
                pre_state: StateHash::deserialize_reader(reader)?,
                msg: read_bounded_bytes(
                    reader,
                    crate::execution::MAX_EFFECT_PAYLOAD_BYTES,
                    "public event payload",
                )?,
            }),
            tag => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown public event tag {tag}"),
            )),
        }
    }
}

const PRIVATE_EVENT_INPUT_RECEIVED: u8 = 0;
const PRIVATE_EVENT_TIMER_FIRED: u8 = 1;
const PRIVATE_EVENT_TYPED_TIMER_FIRED: u8 = 2;
const PRIVATE_EVENT_SIGNED: u8 = 3;
const PRIVATE_EVENT_REACT: u8 = 4;

impl BorshSerialize for PrivateEvent {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        match self {
            Self::InputReceived {
                callout_index,
                data,
                continuation_tag,
            } => {
                BorshSerialize::serialize(&PRIVATE_EVENT_INPUT_RECEIVED, writer)?;
                BorshSerialize::serialize(callout_index, writer)?;
                serialize_bounded_bytes(
                    writer,
                    data,
                    crate::execution::MAX_EFFECT_PAYLOAD_BYTES,
                    "private event input payload",
                )?;
                BorshSerialize::serialize(continuation_tag, writer)
            }
            Self::TimerFired => BorshSerialize::serialize(&PRIVATE_EVENT_TIMER_FIRED, writer),
            Self::TypedTimerFired { timer } => {
                BorshSerialize::serialize(&PRIVATE_EVENT_TYPED_TIMER_FIRED, writer)?;
                timer.serialize_bounded(writer)
            }
            Self::Signed {
                signature,
                continuation_tag,
            } => {
                BorshSerialize::serialize(&PRIVATE_EVENT_SIGNED, writer)?;
                serialize_bounded_bytes(
                    writer,
                    signature,
                    crate::execution::MAX_EFFECT_PAYLOAD_BYTES,
                    "private event signature payload",
                )?;
                BorshSerialize::serialize(continuation_tag, writer)
            }
            Self::React => BorshSerialize::serialize(&PRIVATE_EVENT_REACT, writer),
        }
    }
}

impl BorshDeserialize for PrivateEvent {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            PRIVATE_EVENT_INPUT_RECEIVED => Ok(Self::InputReceived {
                callout_index: u32::deserialize_reader(reader)?,
                data: read_bounded_bytes(
                    reader,
                    crate::execution::MAX_EFFECT_PAYLOAD_BYTES,
                    "private input payload",
                )?,
                continuation_tag: Option::<u32>::deserialize_reader(reader)?,
            }),
            PRIVATE_EVENT_TIMER_FIRED => Ok(Self::TimerFired),
            PRIVATE_EVENT_TYPED_TIMER_FIRED => Ok(Self::TypedTimerFired {
                timer: TimerPayload::deserialize_bounded(reader)?,
            }),
            PRIVATE_EVENT_SIGNED => Ok(Self::Signed {
                signature: read_bounded_bytes(
                    reader,
                    crate::execution::MAX_EFFECT_PAYLOAD_BYTES,
                    "private signature payload",
                )?,
                continuation_tag: Option::<u32>::deserialize_reader(reader)?,
            }),
            PRIVATE_EVENT_REACT => Ok(Self::React),
            tag => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown private event tag {tag}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn borsh_round_trip_all_variants() {
        let peer = PeerId([1u8; 32]);

        let variants: Vec<Event> = vec![
            Event::SessionStarted {
                ensemble: Ensemble::from_peers(vec![peer, PeerId([0u8; 32])]).expect("ensemble"),
            },
            Event::MessageReceived {
                message_id: MessageId([9u8; 32]),
                from: peer,
                position: 3,
                pre_state: StateHash([8u8; 32]),
                msg: vec![1, 2, 3],
            },
            Event::InputReceived {
                callout_index: 0,
                data: vec![4, 5, 6],
                continuation_tag: None,
            },
            Event::TimerFired,
            Event::TypedTimerFired {
                timer: TimerPayload {
                    type_name: "Timer".into(),
                    data: vec![7],
                },
            },
            Event::Signed {
                signature: vec![0xDD, 0xEE],
                continuation_tag: None,
            },
            Event::React,
        ];

        for variant in &variants {
            let encoded = borsh::to_vec(variant).expect("serialize");
            let decoded: Event = borsh::from_slice(&encoded).expect("deserialize");
            assert_eq!(*variant, decoded);
        }
    }

    #[test]
    fn persisted_event_tags_are_fixed_and_unknown_tags_are_rejected() {
        let peer = PeerId([1; 32]);
        let ensemble = Ensemble::from_peers(vec![peer, PeerId([2; 32])]).expect("ensemble");
        let public_start = PublicEvent::SessionStarted { ensemble };
        let public_message = PublicEvent::MessageReceived {
            message_id: MessageId([2; 32]),
            from: peer,
            position: 0,
            pre_state: StateHash([3; 32]),
            msg: Vec::new(),
        };
        let private_timer = PrivateEvent::TimerFired;
        let raw_react: Event<Vec<u8>> = Event::React;
        assert_eq!(borsh::to_vec(&public_start).unwrap()[0], 0);
        assert_eq!(borsh::to_vec(&public_message).unwrap()[0], 1);
        assert_eq!(borsh::to_vec(&private_timer).unwrap()[0], 1);
        assert_eq!(borsh::to_vec(&raw_react).unwrap()[0], 6);
        assert!(borsh::from_slice::<PublicEvent>(&[0xff]).is_err());
        assert!(borsh::from_slice::<PrivateEvent>(&[0xff]).is_err());
        assert!(borsh::from_slice::<Event<Vec<u8>>>(&[0xff]).is_err());
    }

    #[test]
    fn typed_timer_encodings_keep_bounds_across_events_and_effects() {
        let timer = TimerPayload {
            type_name: "T".into(),
            data: vec![7],
        };
        let event = PrivateEvent::TypedTimerFired {
            timer: timer.clone(),
        };
        let event_bytes = [2, 1, 0, 0, 0, b'T', 1, 0, 0, 0, 7];
        assert_eq!(borsh::to_vec(&event).unwrap(), event_bytes);
        assert_eq!(PrivateEvent::try_from_slice(&event_bytes).unwrap(), event);

        let effect = crate::PrivateEffect::SetTimer {
            delay_ms: 0,
            timer: Some(timer.clone()),
        };
        let mut effect_bytes = vec![1, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        effect_bytes.extend_from_slice(&event_bytes[1..]);
        assert_eq!(borsh::to_vec(&effect).unwrap(), effect_bytes);
        assert_eq!(
            crate::PrivateEffect::try_from_slice(&effect_bytes).unwrap(),
            effect
        );
        let raw_effect = crate::Effect::SetTimer {
            delay_ms: 0,
            timer: Some(timer),
        };
        effect_bytes[0] = 4;
        assert_eq!(borsh::to_vec(&raw_effect).unwrap(), effect_bytes);
        assert_eq!(
            crate::Effect::try_from_slice(&effect_bytes).unwrap(),
            raw_effect
        );

        let oversized_name = u32::try_from(crate::execution::MAX_TERMINAL_REASON_BYTES + 1)
            .unwrap()
            .to_le_bytes();
        let oversized_data = u32::try_from(crate::execution::MAX_TIMER_PAYLOAD_BYTES + 1)
            .unwrap()
            .to_le_bytes();
        for payload in [
            oversized_name.to_vec(),
            [0u32.to_le_bytes(), oversized_data].concat(),
        ] {
            let mut encoded = vec![2];
            encoded.extend_from_slice(&payload);
            assert_eq!(
                PrivateEvent::try_from_slice(&encoded).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            encoded = vec![1, 0, 0, 0, 0, 0, 0, 0, 0, 1];
            encoded.extend_from_slice(&payload);
            assert_eq!(
                crate::PrivateEffect::try_from_slice(&encoded)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
    }
}
