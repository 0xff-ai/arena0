//! Trace entry types: the public per-position record, pending continuation
//! metadata shared by the live dispatch path and replay verifier.

use crate::PendingId;
use crate::bounded::{read_option_string, write_option_string};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use std::io;

use crate::{PrivateEffect, PublicEffect, PublicEvent, StateHash};

use super::commitment::AggregateAttestation;
use super::private::WitnessCommitment;

/// One entry in the public trace section, negotiating a link in the proof chain.
///
/// The co-signed projection is byte-identical on every node: entries record
/// shared events (boundaries and broadcast messages) applied at canonical
/// positions, the effects they produce, deterministic fuel consumption, and
/// the sender's witness commitment for message entries. `pre_state` of
/// position N+1 must equal `post_state` of position N; verifiers check this
/// invariant to confirm trace integrity. `step` is the canonical public
/// position, identical across nodes.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TraceEntry {
    /// Trace schema version for this entry.
    pub trace_version: u32,
    /// Zero-based canonical public position within the session.
    pub step: u64,
    /// The shared event applied at this position.
    pub event: PublicEvent,
    /// The effects the shared handler emitted in response.
    pub effects: Vec<PublicEffect>,
    /// BLAKE3 hash of shared state before this entry.
    pub pre_state: StateHash,
    /// BLAKE3 hash of shared state after this entry.
    pub post_state: StateHash,
    /// Fuel consumed by this fresh deterministic guest call. The execution
    /// profile, explicit state, and event fully determine this value, so it is
    /// part of the co-signed entry content and must match during replay.
    pub fuel_used: u64,
    /// For message entries, the sender's blake3 commitment to the local
    /// witness that produced the message. Travels with the message so all
    /// nodes record identical bytes; `None` for boundary entries.
    pub witness: Option<WitnessCommitment>,
    /// The BLS aggregate agreement recorded at this position: the aggregate
    /// over the signing participants plus a bitmap of who signed. Joined from
    /// the signature log at read time; excluded from the entry hash.
    pub agreement: AggregateAttestation,
}

impl BorshSerialize for TraceEntry {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        if self.effects.len() > crate::execution::MAX_SHARED_EFFECTS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "shared effect count exceeds bound",
            ));
        }
        BorshSerialize::serialize(&self.trace_version, writer)?;
        BorshSerialize::serialize(&self.step, writer)?;
        BorshSerialize::serialize(&self.event, writer)?;
        let count = u32::try_from(self.effects.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "effect count overflows u32")
        })?;
        BorshSerialize::serialize(&count, writer)?;
        for effect in &self.effects {
            BorshSerialize::serialize(effect, writer)?;
        }
        BorshSerialize::serialize(&self.pre_state, writer)?;
        BorshSerialize::serialize(&self.post_state, writer)?;
        BorshSerialize::serialize(&self.fuel_used, writer)?;
        BorshSerialize::serialize(&self.witness, writer)?;
        BorshSerialize::serialize(&self.agreement, writer)
    }
}

impl BorshDeserialize for TraceEntry {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        let trace_version = u32::deserialize_reader(reader)?;
        let step = u64::deserialize_reader(reader)?;
        let event = PublicEvent::deserialize_reader(reader)?;
        let count = u32::deserialize_reader(reader)? as usize;
        if count > crate::execution::MAX_SHARED_EFFECTS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "shared effect count exceeds bound",
            ));
        }
        let mut effects = Vec::with_capacity(count);
        for _ in 0..count {
            effects.push(PublicEffect::deserialize_reader(reader)?);
        }
        Ok(Self {
            trace_version,
            step,
            event,
            effects,
            pre_state: StateHash::deserialize_reader(reader)?,
            post_state: StateHash::deserialize_reader(reader)?,
            fuel_used: u64::deserialize_reader(reader)?,
            witness: Option::<WitnessCommitment>::deserialize_reader(reader)?,
            agreement: AggregateAttestation::deserialize_reader(reader)?,
        })
    }
}

impl TraceEntry {
    /// Whether this entry ends execution according to its event or real guest
    /// effects. Program phase names and handler return values are guest state,
    /// not a second host-visible lifecycle representation.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.effects.iter().any(|effect| {
            matches!(
                effect,
                PublicEffect::SessionEnd { .. }
                    | PublicEffect::SessionAbort { .. }
                    | PublicEffect::Fail { .. }
            )
        })
    }

    /// The successful outcome carried by this entry, if any.
    #[must_use]
    pub fn completed_outcome(&self) -> Option<&[u8]> {
        self.effects.iter().find_map(|effect| match effect {
            PublicEffect::SessionEnd { outcome } => Some(outcome.as_slice()),
            _ => None,
        })
    }

    /// The abort or guest-failure reason carried by this entry, if any.
    #[must_use]
    pub fn abort_reason(&self) -> Option<&str> {
        self.effects.iter().find_map(|effect| match effect {
            PublicEffect::SessionAbort { reason } | PublicEffect::Fail { reason } => {
                Some(reason.as_str())
            }
            _ => None,
        })
    }

    /// The blake3 hash of this entry's canonical bytes, excluding only the
    /// agreement (a log join, not entry content).
    /// This is the entry-content commitment bound into the signed
    /// [`StepCommitment`](super::commitment::StepCommitment): it covers the
    /// position, event bytes, effects, pre/post state, fuel, and witness, so the
    /// complete deterministic shared transition is co-signed.
    #[must_use]
    pub fn entry_hash(&self) -> [u8; 32] {
        let mut canonical = self.clone();
        canonical.agreement = AggregateAttestation::empty();
        let bytes = borsh::to_vec(&canonical).expect("TraceEntry is always serializable");
        *blake3::hash(&bytes).as_bytes()
    }
}

/// Runtime trace metadata for a suspended continuation or pending external callout.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PendingRecord {
    /// Stable id for this pending point within one local execution trace.
    pub id: PendingId,
    /// Kind of external result that can resume the pending point.
    pub operation: PendingOperation,
    /// Optional author-declared local pending label.
    pub label: Option<String>,
    /// Human-readable expected result type, when known by generated code.
    pub expected_type: Option<String>,
    /// Generated local continuation tag for restoring the right resume point.
    pub continuation_tag: Option<u32>,
}

impl BorshSerialize for PendingRecord {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        BorshSerialize::serialize(&self.id, writer)?;
        BorshSerialize::serialize(&self.operation, writer)?;
        serialize_pending_string(writer, self.label.as_deref())?;
        serialize_pending_string(writer, self.expected_type.as_deref())?;
        BorshSerialize::serialize(&self.continuation_tag, writer)
    }
}

impl BorshDeserialize for PendingRecord {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        Ok(Self {
            id: PendingId::deserialize_reader(reader)?,
            operation: PendingOperation::deserialize_reader(reader)?,
            label: read_option_string(
                reader,
                crate::execution::MAX_TERMINAL_REASON_BYTES,
                "pending label",
            )?,
            expected_type: read_option_string(
                reader,
                crate::execution::MAX_TERMINAL_REASON_BYTES,
                "pending expected type",
            )?,
            continuation_tag: Option::<u32>::deserialize_reader(reader)?,
        })
    }
}

fn serialize_pending_string<W: borsh::io::Write>(
    writer: &mut W,
    value: Option<&str>,
) -> io::Result<()> {
    // Pending records reject an oversized field before writing its option tag.
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

/// The operation whose answer resumes one durable continuation.
#[derive(
    BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
pub enum PendingOperation {
    /// A typed callout always identifies its program variant.
    Callout { callout_index: u32 },
    /// A signing continuation has no callout index.
    Sign,
}

impl PendingOperation {
    /// The class of external answer this operation consumes.
    #[must_use]
    pub const fn kind(self) -> PendingKind {
        match self {
            Self::Callout { .. } => PendingKind::Callout,
            Self::Sign => PendingKind::Sign,
        }
    }
}

impl PendingRecord {
    /// Build pending metadata from one effect that can suspend execution.
    #[must_use]
    pub fn from_effect(id: PendingId, effect: &PrivateEffect) -> Option<Self> {
        match effect {
            PrivateEffect::Callout {
                callout_index,
                pending_label,
                expected_type,
                continuation_tag,
                ..
            } => Some(Self {
                id,
                operation: PendingOperation::Callout {
                    callout_index: *callout_index,
                },
                label: pending_label.clone(),
                expected_type: expected_type.clone(),
                continuation_tag: *continuation_tag,
            }),
            PrivateEffect::Sign {
                pending_label,
                expected_type,
                continuation_tag,
                ..
            } => Some(Self {
                id,
                operation: PendingOperation::Sign,
                label: pending_label.clone(),
                expected_type: expected_type.clone().or_else(|| Some("Vec<u8>".into())),
                continuation_tag: *continuation_tag,
            }),
            _ => None,
        }
    }

    /// Build pending metadata from the first suspending effect in a step.
    #[must_use]
    pub fn from_effects(id: PendingId, effects: &[PrivateEffect]) -> Option<Self> {
        effects
            .iter()
            .find_map(|effect| Self::from_effect(id, effect))
    }
}

/// Class of external result that can resume a pending point.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PendingKind {
    /// Waiting for a typed input answer to a program callout.
    Callout,
    /// Waiting for a host signature.
    Sign,
}

impl BorshSerialize for PendingKind {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> std::io::Result<()> {
        BorshSerialize::serialize(
            &match self {
                Self::Callout => 0u8,
                Self::Sign => 1u8,
            },
            writer,
        )
    }
}

impl BorshDeserialize for PendingKind {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> std::io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            0 => Ok(Self::Callout),
            1 => Ok(Self::Sign),
            tag => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown pending kind tag {tag}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0_crypto::BlsSignature;

    use crate::{MessageId, PeerId, SignerSet};

    #[test]
    fn pending_record_from_callout_effect() {
        let record = PendingRecord::from_effect(
            PendingId::new(7),
            &PrivateEffect::Callout {
                callout_index: 3,
                context: vec![1, 2, 3],
                pending_label: Some("thinking".into()),
                expected_type: Some("Choice".into()),
                continuation_tag: Some(11),
            },
        )
        .expect("callout effect should suspend");

        assert_eq!(record.id, PendingId::new(7));
        assert_eq!(
            record.operation,
            PendingOperation::Callout { callout_index: 3 }
        );
        assert_eq!(record.continuation_tag, Some(11));
    }

    #[test]
    fn entry_hash_ignores_agreement_but_binds_content() {
        let entry = TraceEntry {
            trace_version: crate::TRACE_FORMAT_VERSION,
            step: 4,
            event: PublicEvent::MessageReceived {
                message_id: MessageId([4; 32]),
                from: PeerId([1; 32]),
                position: 4,
                pre_state: StateHash([1; 32]),
                msg: Vec::new(),
            },
            effects: Vec::new(),
            pre_state: StateHash([1; 32]),
            post_state: StateHash([2; 32]),
            fuel_used: 9,
            witness: Some(WitnessCommitment([7; 32])),
            agreement: AggregateAttestation::empty(),
        };
        let hash = entry.entry_hash();

        let mut signed = entry.clone();
        signed.agreement = AggregateAttestation {
            aggregate: BlsSignature([3; 48]),
            signers: {
                let mut s = SignerSet::with_capacity(2);
                s.set(0);
                s
            },
        };
        assert_eq!(hash, signed.entry_hash());

        // Fuel is deterministic for a fresh call and is signed evidence.
        let mut refueled = entry.clone();
        refueled.fuel_used += 1;
        assert_ne!(hash, refueled.entry_hash());

        // Witness and post-state ARE co-signed content: changing them changes the hash.
        let mut tampered = entry.clone();
        tampered.witness = None;
        assert_ne!(hash, tampered.entry_hash());
        let mut tampered = entry;
        tampered.post_state = StateHash([9; 32]);
        assert_ne!(hash, tampered.entry_hash());
    }
    #[test]
    fn pending_operations_have_exact_bounded_encodings() {
        for operation in [
            PendingOperation::Callout { callout_index: 3 },
            PendingOperation::Sign,
        ] {
            let record = PendingRecord {
                id: PendingId::new(u64::MAX),
                operation,
                label: None,
                expected_type: None,
                continuation_tag: None,
            };
            let mut expected = u64::MAX.to_le_bytes().to_vec();
            match operation {
                PendingOperation::Callout { callout_index } => {
                    expected.push(0);
                    expected.extend_from_slice(&callout_index.to_le_bytes());
                }
                PendingOperation::Sign => expected.push(1),
            }
            expected.extend_from_slice(&[0, 0, 0]);
            assert_eq!(borsh::to_vec(&record).unwrap(), expected);
            assert_eq!(PendingRecord::try_from_slice(&expected).unwrap(), record);
            assert_eq!(
                serde_json::to_value(&record).unwrap()["id"],
                u64::MAX.to_string()
            );
            let mut unknown = expected.clone();
            unknown[8] = 2;
            assert!(PendingRecord::try_from_slice(&unknown).is_err());
            if matches!(operation, PendingOperation::Callout { .. }) {
                assert!(PendingRecord::try_from_slice(&expected[..9]).is_err());
            }
            let mut oversized = record;
            oversized.label = Some("x".repeat(crate::execution::MAX_TERMINAL_REASON_BYTES + 1));
            assert!(borsh::to_vec(&oversized).is_err());
        }
    }
}
