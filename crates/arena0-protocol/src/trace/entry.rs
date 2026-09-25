//! Portable trace entries.

use crate::bounded::{
    read_bytes as read_bounded_bytes, read_string as read_bounded_string,
    write_bytes as serialize_bounded_bytes, write_string as serialize_bounded_string,
};
use crate::{Effect, Ensemble, Event, MessageId, PeerId, SessionHash, StateHash};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use std::io;

use super::commitment::AggregateAttestation;

/// The portable, agreed event of one step.
///
/// Only the two events every participant observes at the same position are
/// representable here. The type makes a non-agreed event in a portable entry
/// unrepresentable, so no runtime shape check is needed.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
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
        data: Vec<u8>,
    },
}

/// The terminal value of one step, if that step ended the session.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum StepTerminal {
    /// Successful completion with opaque outcome bytes.
    End {
        /// Opaque outcome bytes.
        outcome: Vec<u8>,
    },
    /// A shared program abort.
    Abort {
        /// Human-readable reason.
        reason: String,
    },
    /// A shared program failure.
    Fail {
        /// Human-readable reason.
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

    /// The lifecycle effect this terminal value represents.
    #[must_use]
    pub fn to_effect(&self) -> Effect {
        match self {
            Self::End { outcome } => Effect::SessionEnd {
                outcome: outcome.clone(),
            },
            Self::Abort { reason } => Effect::SessionAbort {
                reason: reason.clone(),
            },
            Self::Fail { reason } => Effect::Fail {
                reason: reason.clone(),
            },
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
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
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

impl BorshSerialize for StepEvent {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        match self {
            Self::SessionStarted { ensemble } => {
                BorshSerialize::serialize(&0u8, writer)?;
                BorshSerialize::serialize(ensemble, writer)
            }
            Self::Message { from, data } => {
                BorshSerialize::serialize(&1u8, writer)?;
                BorshSerialize::serialize(from, writer)?;
                serialize_bounded_bytes(
                    writer,
                    data,
                    crate::execution::MAX_EFFECT_PAYLOAD_BYTES,
                    "step message payload",
                )
            }
        }
    }
}

impl BorshDeserialize for StepEvent {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            0 => Ok(Self::SessionStarted {
                ensemble: BorshDeserialize::deserialize_reader(reader)?,
            }),
            1 => Ok(Self::Message {
                from: BorshDeserialize::deserialize_reader(reader)?,
                data: read_bounded_bytes(
                    reader,
                    crate::execution::MAX_EFFECT_PAYLOAD_BYTES,
                    "step message payload",
                )?,
            }),
            tag => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown step event tag {tag}"),
            )),
        }
    }
}

impl BorshSerialize for StepTerminal {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        match self {
            Self::End { outcome } => {
                BorshSerialize::serialize(&0u8, writer)?;
                serialize_bounded_bytes(
                    writer,
                    outcome,
                    crate::execution::MAX_TERMINAL_OUTCOME_BYTES,
                    "terminal outcome",
                )
            }
            Self::Abort { reason } => {
                BorshSerialize::serialize(&1u8, writer)?;
                serialize_bounded_string(
                    writer,
                    reason,
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "terminal reason",
                )
            }
            Self::Fail { reason } => {
                BorshSerialize::serialize(&2u8, writer)?;
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

impl BorshDeserialize for StepTerminal {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            0 => Ok(Self::End {
                outcome: read_bounded_bytes(
                    reader,
                    crate::execution::MAX_TERMINAL_OUTCOME_BYTES,
                    "terminal outcome",
                )?,
            }),
            1 => Ok(Self::Abort {
                reason: read_bounded_string(
                    reader,
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "terminal reason",
                )?,
            }),
            2 => Ok(Self::Fail {
                reason: read_bounded_string(
                    reader,
                    crate::execution::MAX_TERMINAL_REASON_BYTES,
                    "failure reason",
                )?,
            }),
            tag => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown step terminal tag {tag}"),
            )),
        }
    }
}

impl BorshSerialize for TraceEntry {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        validate_version(self.trace_version)?;
        BorshSerialize::serialize(&self.trace_version, writer)?;
        BorshSerialize::serialize(&self.step, writer)?;
        BorshSerialize::serialize(&self.event, writer)?;
        BorshSerialize::serialize(&self.pre_state, writer)?;
        BorshSerialize::serialize(&self.post_state, writer)?;
        BorshSerialize::serialize(&self.terminal, writer)?;
        BorshSerialize::serialize(&self.agreement, writer)
    }
}

impl BorshDeserialize for TraceEntry {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        let entry = Self {
            trace_version: u32::deserialize_reader(reader)?,
            step: u64::deserialize_reader(reader)?,
            event: StepEvent::deserialize_reader(reader)?,
            pre_state: StateHash::deserialize_reader(reader)?,
            post_state: StateHash::deserialize_reader(reader)?,
            terminal: Option::<StepTerminal>::deserialize_reader(reader)?,
            agreement: AggregateAttestation::deserialize_reader(reader)?,
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
        let mut value = entry(StepEvent::SessionStarted {
            ensemble: Ensemble::from_peers(vec![PeerId([1; 32]), PeerId([2; 32])]).unwrap(),
        });
        value.trace_version = 1;
        assert!(borsh::to_vec(&value).is_err());

        value.trace_version = crate::TRACE_FORMAT_VERSION;
        let mut encoded = borsh::to_vec(&value).unwrap();
        encoded[..std::mem::size_of::<u32>()].copy_from_slice(&1u32.to_le_bytes());
        assert!(TraceEntry::try_from_slice(&encoded).is_err());
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
    fn terminal_round_trips_through_its_lifecycle_effect() {
        for effect in [
            Effect::SessionEnd { outcome: vec![7] },
            Effect::SessionAbort {
                reason: "stop".into(),
            },
            Effect::Fail {
                reason: "fail".into(),
            },
        ] {
            let terminal = StepTerminal::from_effect(&effect).expect("lifecycle effect");
            assert_eq!(terminal.to_effect(), effect);
        }
        assert!(StepTerminal::from_effect(&Effect::Broadcast { data: vec![] }).is_none());
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
