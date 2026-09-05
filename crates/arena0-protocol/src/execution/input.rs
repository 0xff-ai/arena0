use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::StateHash;
use crate::bounded::read_string as read_bounded_string;

use super::{
    AbortOccurrence, ExecutionState, MAX_EXECUTION_INPUT_BYTES, MAX_TERMINAL_REASON_BYTES,
    OccurrenceDigest, OccurrenceEvidence, OccurrenceKey, OccurrenceKind, ParticipantStepSignature,
    ParticipantTerminalSignature, PrivateDelta, ProtocolError, ReceiptBody, SharedDelta,
    ensure_encoded, ensure_payload, validate_receipt_body_shape,
};

/// Explicit input to the pure execution reducer.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ExecutionInput {
    /// Complete admission and activate the already-bound session.
    Activate,
    /// Start one public proposal from a local trace result.
    ProposeShared(SharedDelta),
    /// Add one participant's step signature to the pending proposal.
    StepSignature(ParticipantStepSignature),
    /// Commit one local private record and its typed consequences.
    Private(PrivateDelta),
    /// Add one participant's terminal signature.
    TerminalSignature(ParticipantTerminalSignature),
    /// Supply the complete receipt body after terminal certification.
    ReceiptBody(Box<ReceiptBody>),
    /// Accept a signed local or peer abort/fail occurrence.  The sender and
    /// terminal kind are authenticated by the occurrence itself; there is no
    /// separate `PeerAbort` or unsigned `Fail` path.
    Abort(AbortOccurrence),
    /// Mark an in-flight successful terminal proof incomplete while retaining
    /// its collected evidence and publishing no receipt.
    InterruptTerminal(String),
}

impl BorshSerialize for ExecutionInput {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> std::io::Result<()> {
        match self {
            Self::Activate => BorshSerialize::serialize(&0u8, writer),
            Self::ProposeShared(delta) => {
                BorshSerialize::serialize(&1u8, writer)?;
                BorshSerialize::serialize(delta, writer)
            }
            Self::StepSignature(signature) => {
                BorshSerialize::serialize(&2u8, writer)?;
                BorshSerialize::serialize(signature, writer)
            }
            Self::Private(delta) => {
                BorshSerialize::serialize(&3u8, writer)?;
                BorshSerialize::serialize(delta, writer)
            }
            Self::TerminalSignature(signature) => {
                BorshSerialize::serialize(&4u8, writer)?;
                BorshSerialize::serialize(signature, writer)
            }
            Self::ReceiptBody(body) => {
                BorshSerialize::serialize(&5u8, writer)?;
                BorshSerialize::serialize(body, writer)
            }
            Self::Abort(occurrence) => {
                BorshSerialize::serialize(&7u8, writer)?;
                BorshSerialize::serialize(occurrence, writer)
            }
            Self::InterruptTerminal(reason) => {
                BorshSerialize::serialize(&8u8, writer)?;
                BorshSerialize::serialize(reason, writer)
            }
        }
    }
}

impl BorshDeserialize for ExecutionInput {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> std::io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            0 => Ok(Self::Activate),
            1 => Ok(Self::ProposeShared(SharedDelta::deserialize_reader(
                reader,
            )?)),
            2 => Ok(Self::StepSignature(
                ParticipantStepSignature::deserialize_reader(reader)?,
            )),
            3 => Ok(Self::Private(PrivateDelta::deserialize_reader(reader)?)),
            4 => Ok(Self::TerminalSignature(
                ParticipantTerminalSignature::deserialize_reader(reader)?,
            )),
            5 => Ok(Self::ReceiptBody(Box::new(
                ReceiptBody::deserialize_reader(reader)?,
            ))),
            7 => Ok(Self::Abort(AbortOccurrence::deserialize_reader(reader)?)),
            8 => Ok(Self::InterruptTerminal(read_bounded_string(
                reader,
                MAX_TERMINAL_REASON_BYTES,
                "terminal reason",
            )?)),
            tag => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown execution input tag {tag}"),
            )),
        }
    }
}

impl ExecutionInput {
    /// Decode one bounded input from canonical Borsh bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        ensure_encoded("execution input", bytes.len(), MAX_EXECUTION_INPUT_BYTES)?;
        let input: Self = borsh::from_slice(bytes)
            .map_err(|error| ProtocolError::Deserialization(error.to_string()))?;
        input.validate_shape()?;
        Ok(input)
    }

    /// Encode one input and enforce its total wire bound.
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        self.validate_shape()?;
        let bytes =
            borsh::to_vec(self).map_err(|error| ProtocolError::Serialization(error.to_string()))?;
        ensure_encoded("execution input", bytes.len(), MAX_EXECUTION_INPUT_BYTES)?;
        Ok(bytes)
    }

    /// Return the canonical digest of this complete input.  This digest is
    /// content identity only; it is paired with a semantic [`OccurrenceKey`]
    /// by [`Self::occurrence`].
    pub fn canonical_digest(&self) -> Result<OccurrenceDigest, ProtocolError> {
        self.validate_shape()?;
        Ok(OccurrenceDigest::of(&self.encode()?))
    }

    /// Derive the semantic slot occupied by this input in `state`.
    ///
    /// The key intentionally omits mutable payloads such as reasons,
    /// signatures, and guest bytes.  Reusing one key with another digest is a
    /// typed conflict for the store, not permission to terminate or overwrite
    /// evidence.
    pub fn occurrence_key(&self, state: &ExecutionState) -> Result<OccurrenceKey, ProtocolError> {
        self.validate_shape()?;
        let execution_id = state.execution_id();
        let (kind, coordinate) = match self {
            Self::Activate => (OccurrenceKind::Activate, Vec::new()),
            Self::ProposeShared(delta) => (
                OccurrenceKind::SharedProposal,
                borsh::to_vec(&delta.entry().step)
                    .map_err(|error| ProtocolError::Serialization(error.to_string()))?,
            ),
            Self::StepSignature(signature) => (
                OccurrenceKind::StepSignature,
                borsh::to_vec(&(signature.participant(), signature.signature().step))
                    .map_err(|error| ProtocolError::Serialization(error.to_string()))?,
            ),
            Self::Private(delta) => (
                OccurrenceKind::Private,
                borsh::to_vec(&(delta.execution_id(), delta.record().seq))
                    .map_err(|error| ProtocolError::Serialization(error.to_string()))?,
            ),
            Self::TerminalSignature(signature) => (
                OccurrenceKind::TerminalSignature,
                borsh::to_vec(&(signature.participant(), state.public().next_step()))
                    .map_err(|error| ProtocolError::Serialization(error.to_string()))?,
            ),
            Self::ReceiptBody(_) => (
                OccurrenceKind::ReceiptBody,
                borsh::to_vec(&(state.public().next_step(), state.public().chain_hash()))
                    .map_err(|error| ProtocolError::Serialization(error.to_string()))?,
            ),
            Self::Abort(occurrence) => (
                OccurrenceKind::Abort,
                borsh::to_vec(&(occurrence.sender(), *occurrence.coordinate()))
                    .map_err(|error| ProtocolError::Serialization(error.to_string()))?,
            ),
            Self::InterruptTerminal(_) => (
                OccurrenceKind::InterruptTerminal,
                borsh::to_vec(&(state.public().next_step(), state.public().chain_hash()))
                    .map_err(|error| ProtocolError::Serialization(error.to_string()))?,
            ),
        };
        Ok(OccurrenceKey::derive(execution_id, kind, &coordinate))
    }

    /// Return the key/digest pair used by the reducer plan and the store's
    /// lifetime occurrence table.
    pub fn occurrence(&self, state: &ExecutionState) -> Result<OccurrenceEvidence, ProtocolError> {
        Ok(OccurrenceEvidence::new(
            self.occurrence_key(state)?,
            self.canonical_digest()?,
        ))
    }

    pub(crate) fn validate_shape(&self) -> Result<(), ProtocolError> {
        match self {
            Self::ProposeShared(delta) => {
                delta.validate()?;
                if delta.entry.post_state != StateHash::of(delta.shared_state.as_bytes()) {
                    return Err(ProtocolError::PublicStateHashMismatch);
                }
            }
            Self::Private(delta) => {
                delta.validate()?;
                delta.context.validate()?;
            }
            Self::ReceiptBody(body) => validate_receipt_body_shape(body)?,
            Self::Abort(occurrence) => occurrence.validate_shape()?,
            Self::InterruptTerminal(reason) => {
                ensure_payload("terminal reason", reason.len(), MAX_TERMINAL_REASON_BYTES)?;
            }
            Self::Activate | Self::StepSignature(_) | Self::TerminalSignature(_) => {}
        }
        Ok(())
    }
}
