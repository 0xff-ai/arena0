use crate::PendingId;
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use std::io;

use arena0_crypto::BlsSignature;

use crate::bounded::{read_bytes as read_bounded_bytes, read_string as read_bounded_string};
use crate::trace::{PendingRecord, WitnessCommitment};
use crate::trace::{StepCommitment, TerminalCommitment};
use crate::{ExecId, MessageId, PeerId, StateHash};

use super::{
    AbortOccurrence, ExecutionVersion, GuestSignData, MAX_EFFECT_PAYLOAD_BYTES, MAX_RECEIPT_BYTES,
    MAX_TERMINAL_REASON_BYTES, OUTBOX_DOMAIN, ProtocolError, Receipt, ensure_payload,
};

/// A protocol broadcast's complete durable frame content. The transport layer
/// encodes this typed value; reducers never ask the node to reconstruct it.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct BroadcastFrame {
    message_id: MessageId,
    sequence: u64,
    pre_state: StateHash,
    data: Vec<u8>,
    witness: WitnessCommitment,
}

impl BroadcastFrame {
    pub(crate) fn new(
        message_id: MessageId,
        sequence: u64,
        pre_state: StateHash,
        data: Vec<u8>,
        witness: WitnessCommitment,
    ) -> Result<Self, ProtocolError> {
        ensure_payload("broadcast payload", data.len(), MAX_EFFECT_PAYLOAD_BYTES)?;
        Ok(Self {
            message_id,
            sequence,
            pre_state,
            data,
            witness,
        })
    }

    /// Return the content-addressed message identity.
    #[must_use]
    pub const fn message_id(&self) -> MessageId {
        self.message_id
    }

    /// Return the producer-local sequence/public position carried by the frame.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Return the public pre-state observed by the producer.
    #[must_use]
    pub const fn pre_state(&self) -> StateHash {
        self.pre_state
    }

    /// Borrow the opaque message payload.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Return the producer's private witness commitment.
    #[must_use]
    pub const fn witness(&self) -> WitnessCommitment {
        self.witness
    }

    /// Return a stable frame identity distinct from the outbox occurrence id.
    #[must_use]
    pub fn frame_id(&self) -> FrameId {
        let bytes = borsh::to_vec(self).expect("BroadcastFrame is serializable");
        FrameId::derive(&bytes)
    }

    pub(crate) fn validate(&self) -> Result<(), ProtocolError> {
        ensure_payload(
            "broadcast payload",
            self.data.len(),
            MAX_EFFECT_PAYLOAD_BYTES,
        )
    }
}

/// A protocol frame or local notification payload. Its identity belongs to
/// the frame/message domain, not to the durable outbox occurrence.
#[derive(
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
)]
#[serde(transparent)]
pub struct FrameId([u8; 32]);

impl FrameId {
    /// Derive a frame identity from canonical frame bytes.
    #[must_use]
    pub fn derive(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    /// Construct an id from persisted bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow id bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Durable external work emitted by a reducer. Timer registration is not an
/// effect: it is represented only by [`super::TimerMutation`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum DurableEffect {
    /// Send one complete authenticated broadcast frame to a remote participant.
    SendBroadcast {
        destination: PeerId,
        frame: BroadcastFrame,
    },
    /// Apply the producer's own broadcast through the same frame path as peers.
    ApplyBroadcast { frame: BroadcastFrame },
    /// Ask a local subscriber to receive a typed notification.
    Notify { frame_id: FrameId, payload: Vec<u8> },
    /// Ask a local guest adapter to issue one validated callout.
    RequestCallout {
        pending: PendingRecord,
        context: Vec<u8>,
    },
    /// Ask a local guest adapter to produce one validated signature.
    RequestSignature {
        pending: PendingRecord,
        data: GuestSignData,
    },
    /// Re-issue the durable callout identified by the pending continuation.
    RetryInput {
        pending_id: PendingId,
        reason: String,
    },
    /// Publish an already validated, sealed receipt artifact.
    PublishReceipt { receipt: Box<Receipt> },
    /// Ask the local producer to sign one stable receipt seal preimage.
    RequestProducerSeal { data: super::ReceiptSealData },
    /// Propagate one authenticated abort/fail occurrence to a remote peer.
    SendAbort {
        destination: PeerId,
        occurrence: AbortOccurrence,
    },
    /// Ask the local BLS worker to sign one durably staged shared commitment.
    RequestStepSignature { commitment: StepCommitment },
    /// Publish this host's BLS signature for the exact staged shared commitment.
    PublishStepSignature {
        destination: PeerId,
        commitment: StepCommitment,
        signature: BlsSignature,
    },
    /// Ask the local BLS worker to sign one durably staged terminal commitment.
    RequestTerminalSignature { commitment: TerminalCommitment },
    /// Publish this host's BLS signature for the exact staged terminal commitment.
    PublishTerminalSignature {
        destination: PeerId,
        commitment: TerminalCommitment,
        signature: BlsSignature,
    },
}

impl BorshSerialize for DurableEffect {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        match self {
            Self::SendBroadcast { destination, frame } => {
                BorshSerialize::serialize(&0u8, writer)?;
                BorshSerialize::serialize(destination, writer)?;
                BorshSerialize::serialize(frame, writer)
            }
            Self::ApplyBroadcast { frame } => {
                BorshSerialize::serialize(&1u8, writer)?;
                BorshSerialize::serialize(frame, writer)
            }
            Self::Notify { frame_id, payload } => {
                BorshSerialize::serialize(&2u8, writer)?;
                BorshSerialize::serialize(frame_id, writer)?;
                BorshSerialize::serialize(payload, writer)
            }
            Self::RequestCallout { pending, context } => {
                BorshSerialize::serialize(&3u8, writer)?;
                BorshSerialize::serialize(pending, writer)?;
                BorshSerialize::serialize(context, writer)
            }
            Self::RequestSignature { pending, data } => {
                BorshSerialize::serialize(&4u8, writer)?;
                BorshSerialize::serialize(pending, writer)?;
                BorshSerialize::serialize(data, writer)
            }
            Self::RetryInput { pending_id, reason } => {
                BorshSerialize::serialize(&5u8, writer)?;
                BorshSerialize::serialize(pending_id, writer)?;
                BorshSerialize::serialize(reason, writer)
            }
            Self::PublishReceipt { receipt } => {
                BorshSerialize::serialize(&6u8, writer)?;
                BorshSerialize::serialize(receipt, writer)
            }
            Self::RequestProducerSeal { data } => {
                BorshSerialize::serialize(&7u8, writer)?;
                BorshSerialize::serialize(data, writer)
            }
            Self::SendAbort {
                destination,
                occurrence,
            } => {
                BorshSerialize::serialize(&8u8, writer)?;
                BorshSerialize::serialize(destination, writer)?;
                BorshSerialize::serialize(occurrence, writer)
            }
            Self::RequestStepSignature { commitment } => {
                BorshSerialize::serialize(&9u8, writer)?;
                BorshSerialize::serialize(commitment, writer)
            }
            Self::PublishStepSignature {
                destination,
                commitment,
                signature,
            } => {
                BorshSerialize::serialize(&10u8, writer)?;
                BorshSerialize::serialize(destination, writer)?;
                BorshSerialize::serialize(commitment, writer)?;
                BorshSerialize::serialize(signature, writer)
            }
            Self::RequestTerminalSignature { commitment } => {
                BorshSerialize::serialize(&11u8, writer)?;
                BorshSerialize::serialize(commitment, writer)
            }
            Self::PublishTerminalSignature {
                destination,
                commitment,
                signature,
            } => {
                BorshSerialize::serialize(&12u8, writer)?;
                BorshSerialize::serialize(destination, writer)?;
                BorshSerialize::serialize(commitment, writer)?;
                BorshSerialize::serialize(signature, writer)
            }
        }
    }
}

impl BorshDeserialize for DurableEffect {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            0 => Ok(Self::SendBroadcast {
                destination: PeerId::deserialize_reader(reader)?,
                frame: BroadcastFrame::deserialize_reader(reader)?,
            }),
            1 => Ok(Self::ApplyBroadcast {
                frame: BroadcastFrame::deserialize_reader(reader)?,
            }),
            2 => Ok(Self::Notify {
                frame_id: FrameId::deserialize_reader(reader)?,
                payload: read_bounded_bytes(
                    reader,
                    MAX_EFFECT_PAYLOAD_BYTES,
                    "notification payload",
                )?,
            }),
            3 => Ok(Self::RequestCallout {
                pending: PendingRecord::deserialize_reader(reader)?,
                context: read_bounded_bytes(reader, MAX_EFFECT_PAYLOAD_BYTES, "callout context")?,
            }),
            4 => Ok(Self::RequestSignature {
                pending: PendingRecord::deserialize_reader(reader)?,
                data: GuestSignData::deserialize_reader(reader)?,
            }),
            5 => Ok(Self::RetryInput {
                pending_id: PendingId::deserialize_reader(reader)?,
                reason: read_bounded_string(reader, MAX_TERMINAL_REASON_BYTES, "retry reason")?,
            }),
            6 => Ok(Self::PublishReceipt {
                receipt: Box::new(Receipt::deserialize_reader(reader)?),
            }),
            7 => Ok(Self::RequestProducerSeal {
                data: super::ReceiptSealData::deserialize_reader(reader)?,
            }),
            8 => Ok(Self::SendAbort {
                destination: PeerId::deserialize_reader(reader)?,
                occurrence: AbortOccurrence::deserialize_reader(reader)?,
            }),
            9 => Ok(Self::RequestStepSignature {
                commitment: StepCommitment::deserialize_reader(reader)?,
            }),
            10 => Ok(Self::PublishStepSignature {
                destination: PeerId::deserialize_reader(reader)?,
                commitment: StepCommitment::deserialize_reader(reader)?,
                signature: BlsSignature::deserialize_reader(reader)?,
            }),
            11 => Ok(Self::RequestTerminalSignature {
                commitment: TerminalCommitment::deserialize_reader(reader)?,
            }),
            12 => Ok(Self::PublishTerminalSignature {
                destination: PeerId::deserialize_reader(reader)?,
                commitment: TerminalCommitment::deserialize_reader(reader)?,
                signature: BlsSignature::deserialize_reader(reader)?,
            }),
            tag => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown durable effect tag {tag}"),
            )),
        }
    }
}

impl DurableEffect {
    pub(crate) fn send_broadcast(
        destination: PeerId,
        frame: BroadcastFrame,
    ) -> Result<Self, ProtocolError> {
        frame.validate()?;
        Ok(Self::SendBroadcast { destination, frame })
    }

    pub(crate) fn apply_broadcast(frame: BroadcastFrame) -> Result<Self, ProtocolError> {
        frame.validate()?;
        Ok(Self::ApplyBroadcast { frame })
    }

    /// Construct a local notification effect.
    pub fn notify(frame_id: FrameId, payload: impl Into<Vec<u8>>) -> Result<Self, ProtocolError> {
        let payload = payload.into();
        ensure_payload("effect payload", payload.len(), MAX_EFFECT_PAYLOAD_BYTES)?;
        Ok(Self::Notify { frame_id, payload })
    }

    pub(crate) fn request_callout(
        pending: PendingRecord,
        context: Vec<u8>,
    ) -> Result<Self, ProtocolError> {
        ensure_payload("callout context", context.len(), MAX_EFFECT_PAYLOAD_BYTES)?;
        Ok(Self::RequestCallout { pending, context })
    }

    pub(crate) fn request_signature(
        pending: PendingRecord,
        data: GuestSignData,
    ) -> Result<Self, ProtocolError> {
        data.validate()?;
        Ok(Self::RequestSignature { pending, data })
    }

    pub(crate) fn retry_input(
        pending_id: PendingId,
        reason: String,
    ) -> Result<Self, ProtocolError> {
        ensure_payload("retry reason", reason.len(), MAX_TERMINAL_REASON_BYTES)?;
        Ok(Self::RetryInput { pending_id, reason })
    }

    pub(crate) fn publish_receipt(receipt: Receipt) -> Result<Self, ProtocolError> {
        ensure_payload(
            "receipt payload",
            receipt.encode()?.len(),
            MAX_RECEIPT_BYTES,
        )?;
        Ok(Self::PublishReceipt {
            receipt: Box::new(receipt),
        })
    }

    pub(crate) fn send_abort(
        destination: PeerId,
        occurrence: AbortOccurrence,
    ) -> Result<Self, ProtocolError> {
        occurrence.validate_shape()?;
        Ok(Self::SendAbort {
            destination,
            occurrence,
        })
    }

    pub(crate) fn request_step_signature(commitment: StepCommitment) -> Self {
        Self::RequestStepSignature { commitment }
    }

    pub(crate) fn publish_step_signature(
        destination: PeerId,
        commitment: StepCommitment,
        signature: BlsSignature,
    ) -> Self {
        Self::PublishStepSignature {
            destination,
            commitment,
            signature,
        }
    }

    pub(crate) fn request_terminal_signature(commitment: TerminalCommitment) -> Self {
        Self::RequestTerminalSignature { commitment }
    }

    pub(crate) fn publish_terminal_signature(
        destination: PeerId,
        commitment: TerminalCommitment,
        signature: BlsSignature,
    ) -> Self {
        Self::PublishTerminalSignature {
            destination,
            commitment,
            signature,
        }
    }

    /// Return the payload size governed by the effect's bound.
    #[must_use]
    pub fn payload_len(&self) -> usize {
        match self {
            Self::SendBroadcast { frame, .. } | Self::ApplyBroadcast { frame } => frame.data.len(),
            Self::Notify { payload, .. }
            | Self::RequestCallout {
                context: payload, ..
            } => payload.len(),
            Self::RequestSignature { data, .. } => data.payload().len(),
            Self::RetryInput { reason, .. } => reason.len(),
            Self::PublishReceipt { receipt } => {
                receipt.encode().map_or(usize::MAX, |bytes| bytes.len())
            }
            Self::RequestProducerSeal { data } => {
                borsh::to_vec(data).map_or(usize::MAX, |bytes| bytes.len())
            }
            Self::SendAbort { occurrence, .. } => occurrence.reason().len(),
            Self::RequestStepSignature { commitment } => {
                borsh::to_vec(commitment).map_or(usize::MAX, |bytes| bytes.len())
            }
            Self::RequestTerminalSignature { commitment } => {
                borsh::to_vec(commitment).map_or(usize::MAX, |bytes| bytes.len())
            }
            Self::PublishStepSignature {
                destination: _,
                commitment,
                signature,
            } => borsh::to_vec(&(commitment, signature)).map_or(usize::MAX, |bytes| bytes.len()),
            Self::PublishTerminalSignature {
                destination: _,
                commitment,
                signature,
            } => borsh::to_vec(&(commitment, signature)).map_or(usize::MAX, |bytes| bytes.len()),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::SendBroadcast { frame, .. } | Self::ApplyBroadcast { frame } => frame.validate(),
            Self::Notify { payload, .. } => {
                ensure_payload("effect payload", payload.len(), MAX_EFFECT_PAYLOAD_BYTES)
            }
            Self::RequestCallout { pending, context } => {
                super::validate_pending_record(pending)?;
                ensure_payload("callout context", context.len(), MAX_EFFECT_PAYLOAD_BYTES)
            }
            Self::RequestSignature { pending, data } => {
                super::validate_pending_record(pending)?;
                data.validate()
            }
            Self::RetryInput { reason, .. } => {
                ensure_payload("retry reason", reason.len(), MAX_TERMINAL_REASON_BYTES)
            }
            Self::PublishReceipt { receipt } => ensure_payload(
                "receipt payload",
                receipt.encode()?.len(),
                MAX_RECEIPT_BYTES,
            ),
            Self::RequestProducerSeal { data } => {
                data.validate()?;
                ensure_payload(
                    "producer seal request",
                    self.payload_len(),
                    MAX_EFFECT_PAYLOAD_BYTES,
                )
            }
            Self::SendAbort { occurrence, .. } => occurrence.validate_shape(),
            Self::RequestStepSignature { commitment } => {
                if commitment.domain != crate::STEP_COMMIT_DOMAIN {
                    return Err(ProtocolError::InvalidCertificate(
                        "invalid step-signature request commitment".into(),
                    ));
                }
                Ok(())
            }
            Self::PublishStepSignature {
                destination: _,
                commitment,
                signature: _,
            } => {
                if commitment.domain != crate::STEP_COMMIT_DOMAIN {
                    return Err(ProtocolError::InvalidCertificate(
                        "invalid step-signature publication commitment".into(),
                    ));
                }
                Ok(())
            }
            Self::RequestTerminalSignature { commitment } => {
                if commitment.domain != crate::TERMINAL_DOMAIN {
                    return Err(ProtocolError::InvalidCertificate(
                        "invalid terminal-signature request commitment".into(),
                    ));
                }
                Ok(())
            }
            Self::PublishTerminalSignature {
                commitment,
                destination: _,
                signature: _,
            } => {
                if commitment.domain != crate::TERMINAL_DOMAIN {
                    return Err(ProtocolError::InvalidCertificate(
                        "invalid terminal-signature publication commitment".into(),
                    ));
                }
                Ok(())
            }
        }
    }
}

/// Identity of one occurrence in the durable outbox. It is intentionally
/// different from [`FrameId`] and [`MessageId`]. Retrying one occurrence reuses
/// this exact id.
#[derive(
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
)]
#[serde(transparent)]
pub struct OutboxId([u8; 32]);

impl OutboxId {
    /// Derive an occurrence id from execution, committed version, ordinal,
    /// and canonical effect bytes.
    #[must_use = "derive the durable outbox occurrence identity"]
    pub fn derive(
        execution_id: ExecId,
        version: ExecutionVersion,
        ordinal: u32,
        effect: &DurableEffect,
    ) -> Result<Self, ProtocolError> {
        let effect_bytes = borsh::to_vec(effect)
            .map_err(|error| ProtocolError::Serialization(error.to_string()))?;
        let mut bytes = Vec::with_capacity(OUTBOX_DOMAIN.len() + 32 + 8 + 4 + effect_bytes.len());
        bytes.extend_from_slice(OUTBOX_DOMAIN);
        bytes.extend_from_slice(&execution_id.0);
        bytes.extend_from_slice(&version.get().to_le_bytes());
        bytes.extend_from_slice(&ordinal.to_le_bytes());
        bytes.extend_from_slice(&effect_bytes);
        Ok(Self(*blake3::hash(&bytes).as_bytes()))
    }

    /// Construct an id from persisted bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow id bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// One occurrence persisted in the outbox.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OutboxIntent {
    pub(crate) id: OutboxId,
    pub(crate) execution_id: ExecId,
    pub(crate) version: ExecutionVersion,
    pub(crate) ordinal: u32,
    pub(crate) effect: DurableEffect,
}

impl OutboxIntent {
    pub(crate) fn new(
        execution_id: ExecId,
        version: ExecutionVersion,
        ordinal: u32,
        effect: DurableEffect,
    ) -> Result<Self, ProtocolError> {
        effect.validate()?;
        let id = OutboxId::derive(execution_id, version, ordinal, &effect)?;
        Ok(Self {
            id,
            execution_id,
            version,
            ordinal,
            effect,
        })
    }

    /// Return the occurrence identity used for retry and acknowledgement.
    #[must_use]
    pub const fn id(&self) -> OutboxId {
        self.id
    }

    /// Return the owning execution.
    #[must_use]
    pub const fn execution_id(&self) -> ExecId {
        self.execution_id
    }

    /// Return the committed execution version that emitted this occurrence.
    #[must_use]
    pub const fn version(&self) -> ExecutionVersion {
        self.version
    }

    /// Return the stable ordinal within the plan.
    #[must_use]
    pub const fn ordinal(&self) -> u32 {
        self.ordinal
    }

    /// Borrow the typed durable effect.
    #[must_use]
    pub const fn effect(&self) -> &DurableEffect {
        &self.effect
    }

    /// Validate the derived identity after recovery.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        let expected =
            OutboxId::derive(self.execution_id, self.version, self.ordinal, &self.effect)?;
        if expected != self.id {
            return Err(ProtocolError::InvalidOutboxId { id: self.id });
        }
        Ok(())
    }
}
