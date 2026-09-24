//! Portable trace entries and pending continuation metadata.

use crate::PendingId;
use crate::bounded::{read_option_string, write_option_string};
use crate::{Effect, Event, StateHash};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use std::io;

use super::commitment::AggregateAttestation;

/// One portable entry in the agreed session trace.
///
/// Only the two events that all participants can observe are portable:
/// `SessionStarted` and `MessageReceived`. Participant-specific events and
/// effects remain in the participant's store. A lifecycle effect is retained as the
/// optional terminal value because terminal agreement must bind its kind and
/// payload even when shared state is unchanged. The aggregate agreement is a
/// log join and is intentionally excluded from [`Self::entry_hash`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TraceEntry {
    pub trace_version: u32,
    pub step: u64,
    pub event: Event<Vec<u8>>,
    pub pre_state: StateHash,
    pub post_state: StateHash,
    pub terminal: Option<Effect>,
    pub agreement: AggregateAttestation,
}

impl BorshSerialize for TraceEntry {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        validate_version(self.trace_version)?;
        self.validate_shape()?;
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
            event: Event::<Vec<u8>>::deserialize_reader(reader)?,
            pre_state: StateHash::deserialize_reader(reader)?,
            post_state: StateHash::deserialize_reader(reader)?,
            terminal: Option::<Effect>::deserialize_reader(reader)?,
            agreement: AggregateAttestation::deserialize_reader(reader)?,
        };
        validate_version(entry.trace_version)?;
        entry.validate_shape()?;
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
    /// Reject an event or terminal value that cannot be included in portable
    /// agreement evidence. This is deliberately a shape check; binding to an
    /// activation and checking the expected step belongs to protocol
    /// validation.
    pub(crate) fn validate_shape(&self) -> io::Result<()> {
        match &self.event {
            Event::SessionStarted { .. } | Event::MessageReceived { .. } => {}
            Event::InputReceived { .. } | Event::TimerFired { .. } | Event::React => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "portable trace entry contains a non-agreed event",
                ));
            }
        }
        if let Some(effect) = &self.terminal
            && !matches!(
                effect,
                Effect::SessionEnd { .. } | Effect::SessionAbort { .. } | Effect::Fail { .. }
            )
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "portable trace entry contains a non-lifecycle terminal effect",
            ));
        }
        Ok(())
    }

    /// Whether this entry carries a lifecycle effect.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.terminal.is_some()
    }

    /// The successful outcome carried by this entry, if any.
    #[must_use]
    pub fn completed_outcome(&self) -> Option<&[u8]> {
        match self.terminal.as_ref() {
            Some(Effect::SessionEnd { outcome }) => Some(outcome.as_slice()),
            _ => None,
        }
    }

    /// The abort or guest-failure reason carried by this entry, if any.
    #[must_use]
    pub fn abort_reason(&self) -> Option<&str> {
        match self.terminal.as_ref() {
            Some(Effect::SessionAbort { reason }) | Some(Effect::Fail { reason }) => {
                Some(reason.as_str())
            }
            _ => None,
        }
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

/// Runtime metadata for a suspended callout continuation.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PendingRecord {
    pub id: PendingId,
    pub operation: PendingOperation,
    pub expected_type: Option<String>,
}

impl BorshSerialize for PendingRecord {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        BorshSerialize::serialize(&self.id, writer)?;
        BorshSerialize::serialize(&self.operation, writer)?;
        serialize_pending_string(writer, self.expected_type.as_deref())?;
        Ok(())
    }
}

impl BorshDeserialize for PendingRecord {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        Ok(Self {
            id: PendingId::deserialize_reader(reader)?,
            operation: PendingOperation::deserialize_reader(reader)?,
            expected_type: read_option_string(
                reader,
                crate::execution::MAX_TERMINAL_REASON_BYTES,
                "pending expected type",
            )?,
        })
    }
}

fn serialize_pending_string<W: borsh::io::Write>(
    writer: &mut W,
    value: Option<&str>,
) -> io::Result<()> {
    if value.is_some_and(|value| value.len() > crate::execution::MAX_TERMINAL_REASON_BYTES) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pending string exceeds bound",
        ));
    }
    write_option_string(
        writer,
        value,
        crate::execution::MAX_TERMINAL_REASON_BYTES,
        "pending string",
    )
}

/// The operation whose answer resumes one pending continuation.
#[derive(
    BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
pub enum PendingOperation {
    Callout { callout_index: u32 },
}

impl PendingOperation {
    #[must_use]
    pub const fn kind(self) -> PendingKind {
        match self {
            Self::Callout { .. } => PendingKind::Callout,
        }
    }
}

impl PendingRecord {
    /// Build continuation metadata from one suspending effect.
    #[must_use]
    pub fn from_effect(id: PendingId, effect: &Effect) -> Option<Self> {
        match effect {
            Effect::Callout {
                callout_index,
                expected_type,
                ..
            } => Some(Self {
                id,
                operation: PendingOperation::Callout {
                    callout_index: *callout_index,
                },
                expected_type: expected_type.clone(),
            }),
            _ => None,
        }
    }

    /// Build metadata from the first suspending effect in a dispatch.
    #[must_use]
    pub fn from_effects(id: PendingId, effects: &[Effect]) -> Option<Self> {
        effects
            .iter()
            .find_map(|effect| Self::from_effect(id, effect))
    }
}

/// Class of external answer that resumes a pending continuation.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PendingKind {
    Callout,
}

impl BorshSerialize for PendingKind {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        BorshSerialize::serialize(
            &match self {
                Self::Callout => 0u8,
            },
            writer,
        )
    }
}

impl BorshDeserialize for PendingKind {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            0 => Ok(Self::Callout),
            tag => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown pending kind tag {tag}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Ensemble, MessageId, PeerId, SignerSet};
    use arena0_crypto::BlsSignature;

    fn entry(event: Event<Vec<u8>>) -> TraceEntry {
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
    fn non_agreed_events_are_rejected_from_portable_entries() {
        let value = entry(Event::InputReceived {
            callout_index: 0,
            data: Vec::new(),
        });
        assert!(borsh::to_vec(&value).is_err());
    }

    #[test]
    fn non_lifecycle_terminal_effects_are_rejected() {
        let mut value = entry(Event::SessionStarted {
            ensemble: Ensemble::from_peers(vec![PeerId([1; 32]), PeerId([2; 32])]).unwrap(),
        });
        value.terminal = Some(Effect::Broadcast { data: Vec::new() });
        assert!(borsh::to_vec(&value).is_err());
    }

    #[test]
    fn incompatible_trace_versions_are_rejected_at_the_codec_boundary() {
        let mut value = entry(Event::SessionStarted {
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
    fn agreement_is_excluded_from_entry_hash() {
        let value = entry(Event::MessageReceived {
            message_id: MessageId([3; 32]),
            from: PeerId([4; 32]),
            position: 0,
            pre_state: StateHash([1; 32]),
            msg: vec![5],
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
