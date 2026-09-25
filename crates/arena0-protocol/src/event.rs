//! Inbound events dispatched to programs by the runtime.
//!
//! Each [`Event`] variant represents something that happened outside the
//! program: a session boundary, a message arrival, or a local input or timer.
//! Every event uses the same dispatch path and may affect either state and
//! emit any [`Effect`](crate::Effect).

use arena0_program::bounded;
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::execution::MAX_EFFECT_PAYLOAD_BYTES;
use crate::{Ensemble, PeerId, TimerPayload};

/// An event dispatched to a program during a single execution step.
///
/// The type parameter `M` controls the message payload type. The runtime uses
/// `Event<Vec<u8>>` (raw bytes); the SDK decodes to `Event<M>` where `M` is the
/// program's typed message enum.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub enum Event<M = Vec<u8>> {
    /// A session has been established with a committed participant ensemble.
    /// The session boundary. Portable traces normalize this event at step 0.
    SessionStarted {
        /// Committed participant set in canonical order.
        ensemble: Ensemble,
    },
    /// A broadcast message applied at a canonical agreed position.
    ///
    /// The event carries no trace coordinates: the author does not know its
    /// post-state yet, and receivers reconstruct the same portable entry from
    /// the frame. See [`crate::StepEvent`].
    MessageReceived { from: PeerId, msg: M },
    /// The controlling agent submitted input in response to a callout.
    InputReceived {
        callout_index: u32,
        #[borsh(
            serialize_with = "bounded::write_bytes::<MAX_EFFECT_PAYLOAD_BYTES>",
            deserialize_with = "bounded::read_bytes::<MAX_EFFECT_PAYLOAD_BYTES>"
        )]
        data: Vec<u8>,
    },
    /// A previously set timer fired with its scheduled payload.
    TimerFired { timer: TimerPayload },
}

impl Event<Vec<u8>> {
    /// Decode the message payload from Borsh, converting `Event<Vec<u8>>` to `Event<M>`.
    ///
    /// Non-message variants pass through unchanged. Returns `Err` only when a
    /// `MessageReceived` payload fails to deserialize.
    pub fn decode<M: BorshDeserialize>(self) -> Result<Event<M>, std::io::Error> {
        Ok(match self {
            Self::MessageReceived { from, msg } => Event::MessageReceived {
                from,
                msg: borsh::from_slice(&msg)?,
            },
            Self::SessionStarted { ensemble } => Event::SessionStarted { ensemble },
            Self::InputReceived {
                callout_index,
                data,
            } => Event::InputReceived {
                callout_index,
                data,
            },
            Self::TimerFired { timer } => Event::TimerFired { timer },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn borsh_round_trip_all_variants() {
        let peer = PeerId([1u8; 32]);

        let variants: Vec<Event> = vec![
            Event::SessionStarted {
                ensemble: Ensemble::from_peers(vec![peer, PeerId([0u8; 32])]).expect("ensemble"),
            },
            Event::MessageReceived {
                from: peer,
                msg: vec![1, 2, 3],
            },
            Event::InputReceived {
                callout_index: 0,
                data: vec![4, 5, 6],
            },
            Event::TimerFired {
                timer: TimerPayload {
                    type_name: "Timer".into(),
                    data: vec![7],
                },
            },
        ];

        for variant in &variants {
            let encoded = borsh::to_vec(variant).expect("serialize");
            let decoded: Event = borsh::from_slice(&encoded).expect("deserialize");
            assert_eq!(*variant, decoded);
        }
    }

    #[test]
    fn flat_event_tags_are_fixed_and_unknown_tags_are_rejected() {
        let peer = PeerId([1; 32]);
        let ensemble = Ensemble::from_peers(vec![peer, PeerId([2; 32])]).expect("ensemble");
        let start: Event<Vec<u8>> = Event::SessionStarted { ensemble };
        let message: Event<Vec<u8>> = Event::MessageReceived {
            from: peer,
            msg: Vec::new(),
        };
        let timer: Event<Vec<u8>> = Event::TimerFired {
            timer: TimerPayload::unit(),
        };
        assert_eq!(borsh::to_vec(&start).unwrap()[0], 0);
        assert_eq!(borsh::to_vec(&message).unwrap()[0], 1);
        assert_eq!(borsh::to_vec(&timer).unwrap()[0], 3);
        assert!(borsh::from_slice::<Event<Vec<u8>>>(&[0xff]).is_err());
    }

    #[test]
    fn timer_encodings_keep_bounds_across_events_and_effects() {
        let timer = TimerPayload {
            type_name: "T".into(),
            data: vec![7],
        };
        let event: Event<Vec<u8>> = Event::TimerFired {
            timer: timer.clone(),
        };
        let event_bytes = [3, 1, 0, 0, 0, b'T', 1, 0, 0, 0, 7];
        assert_eq!(borsh::to_vec(&event).unwrap(), event_bytes);
        assert_eq!(Event::try_from_slice(&event_bytes).unwrap(), event);

        let effect = crate::Effect::SetTimer {
            delay_ms: 0,
            timer,
        };
        let mut effect_bytes = vec![3, 0, 0, 0, 0, 0, 0, 0, 0];
        effect_bytes.extend_from_slice(&event_bytes[1..]);
        assert_eq!(borsh::to_vec(&effect).unwrap(), effect_bytes);
        assert_eq!(
            crate::Effect::try_from_slice(&effect_bytes).unwrap(),
            effect
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
            let mut encoded = vec![3];
            encoded.extend_from_slice(&payload);
            assert_eq!(
                Event::<Vec<u8>>::try_from_slice(&encoded)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
            // `Effect::SetTimer` (tag 3) followed by a zero `delay_ms`.
            encoded = vec![3, 0, 0, 0, 0, 0, 0, 0, 0];
            encoded.extend_from_slice(&payload);
            assert_eq!(
                crate::Effect::try_from_slice(&encoded).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }
}
