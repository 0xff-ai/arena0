//! Portable trace entries.

use crate::execution::{
    MAX_EFFECT_PAYLOAD_BYTES, MAX_TERMINAL_OUTCOME_BYTES, MAX_TERMINAL_REASON_BYTES,
};
use crate::{Effect, Ensemble, Event, MessageId, PeerId, SessionHash, StateHash};
use arena0_program::bounded;
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use std::io;

use super::commitment::AggregateAttestation;

/// The portable, agreed event of one step.
///
/// Only the two events every participant observes at the same position are
/// representable here. The type makes a non-agreed event in a portable entry
/// unrepresentable, so no runtime shape check is needed.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub enum StepEvent {
    /// The session boundary at step 0.
    SessionStarted {
        /// Committed participant set in canonical order.
        ensemble: Ensemble,
    },
    /// One participant's broadcast applied at this step.
    Message {
        /// Authenticated author.
        from: PeerId,
        /// Opaque program payload.
        #[borsh(
            serialize_with = "bounded::write_bytes::<MAX_EFFECT_PAYLOAD_BYTES>",
            deserialize_with = "bounded::read_bytes::<MAX_EFFECT_PAYLOAD_BYTES>"
        )]
        data: Vec<u8>,
    },
}

/// The terminal value of one step, if that step ended the session.
///
/// This mirrors the lifecycle [`Effect`] variants but stays a separate type:
/// its Borsh tags (`End` 0, `Abort` 1, `Fail` 2) are part of the trace
/// format, while `Effect`'s tags belong to the guest ABI. It is derived only
/// through [`StepTerminal::from_effect`].
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub enum StepTerminal {
    /// Successful completion with opaque outcome bytes.
    End {
        /// Opaque outcome bytes.
        #[borsh(
            serialize_with = "bounded::write_bytes::<MAX_TERMINAL_OUTCOME_BYTES>",
            deserialize_with = "bounded::read_bytes::<MAX_TERMINAL_OUTCOME_BYTES>"
        )]
        outcome: Vec<u8>,
    },
    /// A shared program abort.
    Abort {
        /// Human-readable reason.
        #[borsh(
            serialize_with = "bounded::write_string::<MAX_TERMINAL_REASON_BYTES>",
            deserialize_with = "bounded::read_string::<MAX_TERMINAL_REASON_BYTES>"
        )]
        reason: String,
    },
    /// A shared program failure.
    Fail {
        /// Human-readable reason.
        #[borsh(
            serialize_with = "bounded::write_string::<MAX_TERMINAL_REASON_BYTES>",
            deserialize_with = "bounded::read_string::<MAX_TERMINAL_REASON_BYTES>"
        )]
        reason: String,
    },
}

impl StepEvent {
    /// The dispatch event every participant runs for this step.
    ///
    /// The guest event carries no trace coordinates; [`TraceEntry`] owns them.
    #[must_use]
    pub fn dispatch_event(&self) -> Event<Vec<u8>> {
        match self {
            Self::SessionStarted { ensemble } => Event::SessionStarted {
                ensemble: ensemble.clone(),
            },
            Self::Message { from, data } => Event::MessageReceived {
                from: *from,
                msg: data.clone(),
            },
        }
    }
}

impl StepTerminal {
    /// The terminal value for a lifecycle effect, or `None` for any other
    /// effect.
    #[must_use]
    pub fn from_effect(effect: &Effect) -> Option<Self> {
        match effect {
            Effect::SessionEnd { outcome } => Some(Self::End {
                outcome: outcome.clone(),
            }),
            Effect::SessionAbort { reason } => Some(Self::Abort {
                reason: reason.clone(),
            }),
            Effect::Fail { reason } => Some(Self::Fail {
                reason: reason.clone(),
            }),
            Effect::Broadcast { .. } | Effect::SetTimer { .. } => None,
        }
    }

    /// The successful outcome bytes, if this terminal completed the session.
    #[must_use]
    pub fn completed_outcome(&self) -> Option<&[u8]> {
        match self {
            Self::End { outcome } => Some(outcome.as_slice()),
            Self::Abort { .. } | Self::Fail { .. } => None,
        }
    }

    /// The abort or failure reason, if this terminal stopped the session.
    #[must_use]
    pub fn abort_reason(&self) -> Option<&str> {
        match self {
            Self::Abort { reason } | Self::Fail { reason } => Some(reason.as_str()),
            Self::End { .. } => None,
        }
    }
}

/// One portable entry in the agreed session trace.
///
/// The entry owns the trace coordinates and the terminal value. The aggregate
/// agreement is a log join and is intentionally excluded from
/// [`Self::entry_hash`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, BorshSerialize)]
pub struct TraceEntry {
    /// Trace schema version.
    pub trace_version: u32,
    /// Canonical agreed position.
    pub step: u64,
    /// The agreed event.
    pub event: StepEvent,
    /// Shared state hash before the entry.
    pub pre_state: StateHash,
    /// Shared state hash after the entry.
    pub post_state: StateHash,
    /// The terminal value when this step ended the session.
    pub terminal: Option<StepTerminal>,
    /// N-of-N agreement over the commitment.
    pub agreement: AggregateAttestation,
}

#[derive(BorshDeserialize)]
struct TraceEntryRaw {
    trace_version: u32,
    step: u64,
    event: StepEvent,
    pre_state: StateHash,
    post_state: StateHash,
    terminal: Option<StepTerminal>,
    agreement: AggregateAttestation,
}

impl BorshDeserialize for TraceEntry {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        let raw = TraceEntryRaw::deserialize_reader(reader)?;
        let entry = Self {
            trace_version: raw.trace_version,
            step: raw.step,
            event: raw.event,
            pre_state: raw.pre_state,
            post_state: raw.post_state,
            terminal: raw.terminal,
            agreement: raw.agreement,
        };
        validate_version(entry.trace_version)?;
        Ok(entry)
    }
}

fn validate_version(version: u32) -> io::Result<()> {
    if version != crate::TRACE_FORMAT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported trace version {version}; expected {}",
                crate::TRACE_FORMAT_VERSION
            ),
        ));
    }
    Ok(())
}

impl TraceEntry {
    /// The content identity of a message step, derived from the entry itself.
    ///
    /// `None` for the session boundary. Callers that need a message identity
    /// (logs, API projections, dedupe) derive it here instead of storing or
    /// transmitting it.
    #[must_use]
    pub fn message_id(&self, session: SessionHash) -> Option<MessageId> {
        match &self.event {
            StepEvent::SessionStarted { .. } => None,
            StepEvent::Message { from, data } => Some(MessageId::derive(
                session,
                *from,
                self.step,
                self.pre_state,
                self.post_state,
                data,
            )),
        }
    }

    /// Whether this entry carries a terminal value.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.terminal.is_some()
    }

    /// Hash the canonical entry content, excluding the agreement join.
    #[must_use]
    pub fn entry_hash(&self) -> [u8; 32] {
        let mut canonical = self.clone();
        canonical.agreement = AggregateAttestation::empty();
        let bytes = borsh::to_vec(&canonical).expect("TraceEntry is always serializable");
        *blake3::hash(&bytes).as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PeerId, SignerSet};
    use arena0_crypto::BlsSignature;

    fn entry(event: StepEvent) -> TraceEntry {
        TraceEntry {
            trace_version: crate::TRACE_FORMAT_VERSION,
            step: 0,
            event,
            pre_state: StateHash([1; 32]),
            post_state: StateHash([2; 32]),
            terminal: None,
            agreement: AggregateAttestation::empty(),
        }
    }

    #[test]
    fn incompatible_trace_versions_are_rejected_at_the_codec_boundary() {
        let value = entry(StepEvent::SessionStarted {
            ensemble: Ensemble::from_peers(vec![PeerId([1; 32]), PeerId([2; 32])]).unwrap(),
        });
        // The derived serializer carries the version verbatim; decode enforces it.
        let mut encoded = borsh::to_vec(&value).unwrap();
        encoded[..std::mem::size_of::<u32>()].copy_from_slice(&1u32.to_le_bytes());
        assert!(TraceEntry::try_from_slice(&encoded).is_err());
    }

    #[test]
    fn step_events_step_terminals_and_entries_round_trip() {
        let ensemble = Ensemble::from_peers(vec![PeerId([1; 32]), PeerId([2; 32])]).unwrap();
        for event in [
            StepEvent::SessionStarted {
                ensemble: ensemble.clone(),
            },
            StepEvent::Message {
                from: PeerId([4; 32]),
                data: vec![5, 6],
            },
        ] {
            assert_eq!(
                borsh::from_slice::<StepEvent>(&borsh::to_vec(&event).unwrap()).unwrap(),
                event
            );
        }
        for terminal in [
            StepTerminal::End { outcome: vec![7] },
            StepTerminal::Abort {
                reason: "stop".into(),
            },
            StepTerminal::Fail {
                reason: "fail".into(),
            },
        ] {
            assert_eq!(
                borsh::from_slice::<StepTerminal>(&borsh::to_vec(&terminal).unwrap()).unwrap(),
                terminal
            );
        }
        let value = entry(StepEvent::Message {
            from: PeerId([4; 32]),
            data: vec![5],
        });
        assert_eq!(
            TraceEntry::try_from_slice(&borsh::to_vec(&value).unwrap()).unwrap(),
            value
        );
    }

    #[test]
    fn message_id_is_derived_from_the_entry_coordinates() {
        let value = entry(StepEvent::Message {
            from: PeerId([4; 32]),
            data: vec![5],
        });
        let session = SessionHash([9; 32]);
        assert_eq!(
            value.message_id(session),
            Some(MessageId::derive(
                session,
                PeerId([4; 32]),
                0,
                StateHash([1; 32]),
                StateHash([2; 32]),
                &[5],
            ))
        );
    }

    #[test]
    fn terminal_is_derived_from_exactly_the_lifecycle_effects() {
        for (effect, terminal) in [
            (
                Effect::SessionEnd { outcome: vec![7] },
                StepTerminal::End { outcome: vec![7] },
            ),
            (
                Effect::SessionAbort {
                    reason: "stop".into(),
                },
                StepTerminal::Abort {
                    reason: "stop".into(),
                },
            ),
            (
                Effect::Fail {
                    reason: "fail".into(),
                },
                StepTerminal::Fail {
                    reason: "fail".into(),
                },
            ),
        ] {
            assert!(effect.is_lifecycle());
            assert_eq!(StepTerminal::from_effect(&effect), Some(terminal));
        }
        for effect in [
            Effect::Broadcast { data: vec![] },
            Effect::SetTimer {
                delay_ms: 1,
                timer: crate::TimerPayload::unit(),
            },
        ] {
            assert!(!effect.is_lifecycle());
            assert!(StepTerminal::from_effect(&effect).is_none());
        }
    }

    #[test]
    fn agreement_is_excluded_from_entry_hash() {
        let value = entry(StepEvent::Message {
            from: PeerId([4; 32]),
            data: vec![5],
        });
        let hash = value.entry_hash();
        let mut agreed = value.clone();
        agreed.agreement = AggregateAttestation {
            aggregate: BlsSignature([6; 48]),
            signers: {
                let mut set = SignerSet::with_capacity(2);
                set.set(0);
                set
            },
        };
        assert_eq!(hash, agreed.entry_hash());
    }
}
