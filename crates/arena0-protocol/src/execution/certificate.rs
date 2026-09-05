use arena0_crypto::BlsSignature;
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use std::io;

use crate::PeerId;
use crate::bounded::{read_bytes as read_bounded_bytes, write_bytes};
use crate::negotiation::MAX_PARAMS_LEN;
use crate::trace::{AggregateAttestation, SessionHeader, StepSig, TerminalCommitment, TraceEntry};

use super::{
    ExecutionBinding, MAX_RECEIPT_BYTES, ProtocolError, SharedProposal, StepCertificate,
    TerminalCertificate, ensure_payload,
};

/// Build the activation-bound N-of-N certificate for a shared proposal.
pub(crate) fn make_step_certificate(
    binding: &ExecutionBinding,
    proposal: &SharedProposal,
) -> Result<StepCertificate, ProtocolError> {
    let participants = binding.participant_keys()?;
    if proposal.signatures.len() != participants.len() {
        return Err(ProtocolError::IncompleteProof {
            actual: proposal.signatures.len(),
            expected: participants.len(),
        });
    }

    let mut signatures = Vec::with_capacity(participants.len());
    let mut signer_set = crate::SignerSet::with_capacity(participants.len());
    for (index, (participant, key)) in participants.iter().enumerate() {
        let Some(signature) = proposal
            .signatures
            .iter()
            .find(|signature| signature.participant == *participant)
        else {
            return Err(ProtocolError::IncompleteProof {
                actual: proposal.signatures.len(),
                expected: participants.len(),
            });
        };
        let valid = key
            .verify(
                &proposal.commitment.signing_bytes(),
                &signature.signature.sig,
            )
            .map_err(|error| ProtocolError::InvalidCertificate(error.to_string()))?;
        if !valid {
            return Err(ProtocolError::InvalidStepSignature {
                participant: *participant,
                step: proposal.commitment.step,
            });
        }
        signer_set.set(index);
        signatures.push(signature.signature.sig);
    }

    let agreement = AggregateAttestation::from_signatures(signer_set, &signatures)
        .map_err(|error| ProtocolError::InvalidCertificate(error.to_string()))?;
    agreement
        .verify_signatures(
            proposal.commitment.step,
            &proposal.commitment.signing_bytes(),
            &participants.iter().map(|(_, key)| *key).collect::<Vec<_>>(),
        )
        .map_err(|error| ProtocolError::InvalidCertificate(error.to_string()))?;

    Ok(StepCertificate {
        commitment: proposal.commitment.clone(),
        agreement,
    })
}

/// Build the activation-bound N-of-N certificate for a terminal commitment.
pub(crate) fn make_terminal_certificate(
    binding: &ExecutionBinding,
    commitment: &TerminalCommitment,
    signatures: &[ParticipantTerminalSignature],
) -> Result<TerminalCertificate, ProtocolError> {
    let participants = binding.participant_keys()?;
    if signatures.len() != participants.len() {
        return Err(ProtocolError::IncompleteProof {
            actual: signatures.len(),
            expected: participants.len(),
        });
    }

    let mut collected = Vec::with_capacity(participants.len());
    let mut signer_set = crate::SignerSet::with_capacity(participants.len());
    for (index, (participant, key)) in participants.iter().enumerate() {
        let Some(signature) = signatures
            .iter()
            .find(|signature| signature.participant == *participant)
        else {
            return Err(ProtocolError::IncompleteProof {
                actual: signatures.len(),
                expected: participants.len(),
            });
        };
        let valid = key
            .verify(&commitment.signing_bytes(), &signature.signature)
            .map_err(|error| ProtocolError::InvalidCertificate(error.to_string()))?;
        if !valid {
            return Err(ProtocolError::InvalidTerminalSignature {
                participant: *participant,
            });
        }
        signer_set.set(index);
        collected.push(signature.signature);
    }

    let agreement = AggregateAttestation::from_signatures(signer_set, &collected)
        .map_err(|error| ProtocolError::InvalidCertificate(error.to_string()))?;
    agreement
        .verify_signatures(
            commitment.final_step,
            &commitment.signing_bytes(),
            &participants.iter().map(|(_, key)| *key).collect::<Vec<_>>(),
        )
        .map_err(|error| ProtocolError::InvalidCertificate(error.to_string()))?;

    Ok(TerminalCertificate {
        commitment: commitment.clone(),
        agreement,
    })
}

/// Bounded portable evidence assembled from authoritative activation and trace rows.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ReceiptBody {
    header: SessionHeader,
    outcome: Vec<u8>,
    params: Vec<u8>,
    trace: Vec<TraceEntry>,
}

#[derive(Deserialize)]
struct ReceiptBodyDeserialize {
    header: SessionHeader,
    outcome: Vec<u8>,
    params: Vec<u8>,
    trace: Vec<TraceEntry>,
}

impl<'de> Deserialize<'de> for ReceiptBody {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = ReceiptBodyDeserialize::deserialize(deserializer)?;
        Self::new(raw.header, raw.outcome, raw.params, raw.trace).map_err(serde::de::Error::custom)
    }
}

const RECEIPT_BODY_VERSION: u8 = 2;
const RECEIPT_VERSION: u8 = 2;

impl ReceiptBody {
    /// Assemble a receipt body from its portable proof fields.
    pub fn new(
        header: SessionHeader,
        outcome: Vec<u8>,
        params: Vec<u8>,
        trace: Vec<TraceEntry>,
    ) -> Result<Self, ProtocolError> {
        ensure_payload(
            "receipt outcome",
            outcome.len(),
            super::MAX_TERMINAL_OUTCOME_BYTES,
        )?;
        ensure_payload("receipt params", params.len(), MAX_PARAMS_LEN)?;
        if trace.len() > super::MAX_RECEIPT_TRACE_ENTRIES {
            return Err(ProtocolError::CollectionTooLarge {
                kind: "receipt trace entries",
                actual: trace.len(),
                max: super::MAX_RECEIPT_TRACE_ENTRIES,
            });
        }
        let body = Self {
            header,
            outcome,
            params,
            trace,
        };
        super::validate_receipt_body_shape(&body)?;
        Ok(body)
    }

    /// Borrow the receipt header.
    #[must_use]
    pub const fn header(&self) -> &SessionHeader {
        &self.header
    }

    /// Borrow the proof-bearing terminal evidence.
    #[must_use]
    pub const fn termination(&self) -> &crate::ReceiptTermination {
        &self.header.terminal
    }

    /// Borrow the opaque terminal outcome bytes.
    #[must_use]
    pub fn outcome(&self) -> &[u8] {
        &self.outcome
    }

    /// Borrow the agreed opaque program parameters.
    #[must_use]
    pub fn params(&self) -> &[u8] {
        &self.params
    }

    /// Borrow the complete public trace.
    #[must_use]
    pub fn trace(&self) -> &[TraceEntry] {
        &self.trace
    }
}

impl BorshSerialize for ReceiptBody {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        if self.outcome.len() > super::MAX_TERMINAL_OUTCOME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "receipt outcome exceeds bound",
            ));
        }
        if self.params.len() > MAX_PARAMS_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "receipt params exceed bound",
            ));
        }
        if self.trace.len() > super::MAX_RECEIPT_TRACE_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "receipt trace exceeds bound",
            ));
        }
        BorshSerialize::serialize(&RECEIPT_BODY_VERSION, writer)?;
        BorshSerialize::serialize(&self.header, writer)?;
        write_bytes(
            writer,
            &self.outcome,
            super::MAX_TERMINAL_OUTCOME_BYTES,
            "receipt outcome",
        )?;
        write_bytes(writer, &self.params, MAX_PARAMS_LEN, "receipt params")?;
        let count = u32::try_from(self.trace.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "receipt trace count overflows u32",
            )
        })?;
        BorshSerialize::serialize(&count, writer)?;
        for entry in &self.trace {
            BorshSerialize::serialize(entry, writer)?;
        }
        Ok(())
    }
}

impl BorshDeserialize for ReceiptBody {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        let version = u8::deserialize_reader(reader)?;
        if version != RECEIPT_BODY_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown receipt body version {version}"),
            ));
        }
        let header = SessionHeader::deserialize_reader(reader)?;
        let outcome =
            read_bounded_bytes(reader, super::MAX_TERMINAL_OUTCOME_BYTES, "receipt outcome")?;
        let params = read_bounded_bytes(reader, MAX_PARAMS_LEN, "receipt params")?;
        let count = u32::deserialize_reader(reader)? as usize;
        if count > super::MAX_RECEIPT_TRACE_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "receipt trace exceeds bound",
            ));
        }
        let mut trace = Vec::with_capacity(count);
        for _ in 0..count {
            trace.push(TraceEntry::deserialize_reader(reader)?);
        }
        let body = Self {
            header,
            outcome,
            params,
            trace,
        };
        super::validate_receipt_body_shape(&body)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        Ok(body)
    }
}

crate::id::id_type! {
    /// Content identity of a versioned receipt or stop-report artifact.
    pub struct ReceiptId
}

impl ReceiptId {
    /// Derive the identity of the exact versioned portable evidence.
    pub fn derive_body(body: &ReceiptBody) -> Result<Self, ProtocolError> {
        let bytes =
            borsh::to_vec(body).map_err(|error| ProtocolError::Serialization(error.to_string()))?;
        ensure_payload("receipt body", bytes.len(), MAX_RECEIPT_BYTES)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"arena0/receipt/v2");
        hasher.update(&[RECEIPT_VERSION]);
        hasher.update(&bytes);
        Ok(Self(*hasher.finalize().as_bytes()))
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

/// Evidence unanimously certified by the session participants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    body: ReceiptBody,
    id: ReceiptId,
}

/// A signed unilateral stop and the certified public prefix it references.
/// It does not claim that every participant observed the same stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopReport {
    body: ReceiptBody,
    id: ReceiptId,
}

impl Receipt {
    /// Borrow the unanimously certified evidence.
    #[must_use]
    pub const fn body(&self) -> &ReceiptBody {
        &self.body
    }

    /// Return the canonical artifact identity.
    #[must_use]
    pub const fn receipt_id(&self) -> ReceiptId {
        self.id
    }
}

impl StopReport {
    /// Borrow the signed stop and its certified public prefix.
    #[must_use]
    pub const fn body(&self) -> &ReceiptBody {
        &self.body
    }

    /// Return this report's content identity.
    #[must_use]
    pub const fn receipt_id(&self) -> ReceiptId {
        self.id
    }
}

/// The portable artifact stored and exchanged by the receipt API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiptArtifact {
    Receipt(Receipt),
    StopReport(StopReport),
}

/// Whether an artifact proves unanimous agreement or reports a unilateral stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptKind {
    Receipt,
    StopReport,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactJson<T> {
    kind: ReceiptKind,
    body: T,
}

impl Serialize for ReceiptArtifact {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ArtifactJson {
            kind: self.kind(),
            body: self.body(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ReceiptArtifact {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let ArtifactJson { kind, body } = ArtifactJson::deserialize(deserializer)?;
        let artifact = Self::new(body).map_err(serde::de::Error::custom)?;
        if artifact.kind() != kind {
            return Err(serde::de::Error::custom(
                "artifact kind does not match terminal evidence",
            ));
        }
        Ok(artifact)
    }
}

impl ReceiptArtifact {
    /// Authenticate all evidence and classify its terminal guarantee.
    pub fn new(body: ReceiptBody) -> Result<Self, ProtocolError> {
        let binding = super::ExecutionBinding::new(body.header().activation.clone())?;
        super::validate_receipt_body(&binding, &body)?;
        let id = ReceiptId::derive_body(&body)?;
        let artifact = match body.termination() {
            crate::ReceiptTermination::Stopped {
                cause: super::StopCause::Authenticated(_),
            } => Self::StopReport(StopReport { body, id }),
            _ => Self::Receipt(Receipt { body, id }),
        };
        artifact.encode()?;
        Ok(artifact)
    }

    /// Borrow the authenticated public evidence.
    #[must_use]
    pub const fn body(&self) -> &ReceiptBody {
        match self {
            Self::Receipt(value) => &value.body,
            Self::StopReport(value) => &value.body,
        }
    }

    /// Return the content identity, independent of the exporting Host.
    #[must_use]
    pub const fn receipt_id(&self) -> ReceiptId {
        match self {
            Self::Receipt(value) => value.id,
            Self::StopReport(value) => value.id,
        }
    }

    /// Return the guarantee represented by this artifact.
    #[must_use]
    pub const fn kind(&self) -> ReceiptKind {
        match self {
            Self::Receipt(_) => ReceiptKind::Receipt,
            Self::StopReport(_) => ReceiptKind::StopReport,
        }
    }

    /// Encode under the portable artifact bound.
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        let bytes =
            borsh::to_vec(self).map_err(|error| ProtocolError::Serialization(error.to_string()))?;
        ensure_payload("receipt", bytes.len(), MAX_RECEIPT_BYTES)?;
        Ok(bytes)
    }

    /// Decode and authenticate one versioned portable artifact.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        ensure_payload("receipt", bytes.len(), MAX_RECEIPT_BYTES)?;
        borsh::from_slice(bytes).map_err(|error| ProtocolError::Deserialization(error.to_string()))
    }
}

impl BorshSerialize for ReceiptArtifact {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        BorshSerialize::serialize(&RECEIPT_VERSION, writer)?;
        BorshSerialize::serialize(self.body(), writer)
    }
}

impl BorshDeserialize for ReceiptArtifact {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        let version = u8::deserialize_reader(reader)?;
        if version != RECEIPT_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported receipt version {version}; expected {RECEIPT_VERSION}"),
            ));
        }
        let body = ReceiptBody::deserialize_reader(reader)?;
        Self::new(body)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
    }
}

/// A signature from one participant over a pending public step.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ParticipantStepSignature {
    pub(crate) participant: PeerId,
    pub(crate) signature: StepSig,
}

impl ParticipantStepSignature {
    /// Construct a participant-labelled step signature.
    #[must_use]
    pub const fn new(participant: PeerId, step: u64, signature: BlsSignature) -> Self {
        Self {
            participant,
            signature: StepSig {
                step,
                sig: signature,
            },
        }
    }

    /// Return the participant identity.
    #[must_use]
    pub const fn participant(&self) -> PeerId {
        self.participant
    }

    /// Borrow the trace-owned signature value.
    #[must_use]
    pub const fn signature(&self) -> &StepSig {
        &self.signature
    }
}

/// A signature from one participant over the terminal commitment.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ParticipantTerminalSignature {
    pub(crate) participant: PeerId,
    pub(crate) signature: BlsSignature,
}

impl ParticipantTerminalSignature {
    /// Construct a participant-labelled terminal signature.
    #[must_use]
    pub const fn new(participant: PeerId, signature: BlsSignature) -> Self {
        Self {
            participant,
            signature,
        }
    }

    /// Return the participant identity.
    #[must_use]
    pub const fn participant(&self) -> PeerId {
        self.participant
    }

    /// Return the signature bytes.
    #[must_use]
    pub const fn signature(&self) -> BlsSignature {
        self.signature
    }
}
