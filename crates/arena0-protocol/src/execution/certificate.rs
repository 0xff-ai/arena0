use arena0_crypto::{BlsSignature, Ed25519Signature, SignScheme};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use std::io;

use crate::bounded::{read_bytes as read_bounded_bytes, write_bytes};
use crate::negotiation::MAX_PARAMS_LEN;
use crate::trace::{AggregateAttestation, SessionHeader, StepSig, TerminalCommitment, TraceEntry};
use crate::{PeerId, SessionHash};

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

/// The exact durable lookup key for one producer's receipt of a session.
///
/// The key belongs to the protocol because it is part of receipt identity at
/// the storage boundary: a session may have one receipt per producer, while a
/// content-addressed [`ReceiptId`] is the secondary lookup identity.
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
pub struct ReceiptKey {
    session_id: SessionHash,
    producer: PeerId,
}

impl ReceiptKey {
    /// Construct a receipt lookup key.
    #[must_use]
    pub const fn new(session_id: SessionHash, producer: PeerId) -> Self {
        Self {
            session_id,
            producer,
        }
    }

    /// Return the session identity.
    #[must_use]
    pub const fn session_id(self) -> SessionHash {
        self.session_id
    }

    /// Return the receipt producer identity.
    #[must_use]
    pub const fn producer(self) -> PeerId {
        self.producer
    }
}

/// The complete portable receipt body assembled before a producer seal.
///
/// This is an assembly value only. The final portable artifact is
/// [`Receipt`], which pairs this body with the validated producer seal.
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

const RECEIPT_BODY_VERSION: u8 = 1;
const RECEIPT_VERSION: u8 = 1;

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

    /// Return the exact protocol lookup key for this receipt.
    #[must_use]
    pub fn key(&self) -> ReceiptKey {
        ReceiptKey::new(self.header.session_hash(), self.header.producer)
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

/// Identity of participant-agreed proof evidence across producer artifacts.
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
pub struct ProofId([u8; 32]);

impl ProofId {
    /// Derive the participant-agreed proof identity, excluding only the local
    /// producer identity from the receipt header.
    #[must_use = "derive the stable identity before requesting a producer seal"]
    pub fn derive(body: &ReceiptBody) -> Result<Self, ProtocolError> {
        // Proof identity names the participant-agreed evidence. Producer is a
        // local artifact fact and belongs only to ReceiptId and the seal. Fuel
        // is deterministic trace evidence and remains in the proof.
        let canonical_proof = (
            &body.header.activation,
            &body.header.terminal,
            &body.outcome,
            &body.params,
            &body.trace,
        );
        let bytes = borsh::to_vec(&canonical_proof)
            .map_err(|error| ProtocolError::Serialization(error.to_string()))?;
        ensure_payload("receipt body", bytes.len(), MAX_RECEIPT_BYTES)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"arena0/proof-evidence/v1");
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

/// Domain-separated, versioned and producer-bound seal preimage.
#[derive(
    BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
pub struct ReceiptSealData {
    domain: [u8; 24],
    version: u16,
    proof_id: ProofId,
    receipt_id: ReceiptId,
    producer: PeerId,
}

impl ReceiptSealData {
    /// Domain tag for producer receipt seals.
    pub const DOMAIN: [u8; 24] = *b"arena0/receipt-seal/v1\0\0";
    /// Version of the producer receipt seal contract.
    pub const VERSION: u16 = 1;

    /// Construct the exact preimage a local producer must sign.
    #[must_use]
    pub const fn new(proof_id: ProofId, receipt_id: ReceiptId, producer: PeerId) -> Self {
        Self {
            domain: Self::DOMAIN,
            version: Self::VERSION,
            proof_id,
            receipt_id,
            producer,
        }
    }

    /// Return the proof identity covered by this seal.
    #[must_use]
    pub const fn proof_id(self) -> ProofId {
        self.proof_id
    }

    /// Return the producer identity covered by this seal.
    #[must_use]
    pub const fn producer(self) -> PeerId {
        self.producer
    }

    /// Return the exact public receipt artifact identity covered by this seal.
    #[must_use]
    pub const fn receipt_id(self) -> ReceiptId {
        self.receipt_id
    }

    /// Return the canonical bytes signed by Ed25519.
    pub fn signing_bytes(&self) -> Result<Vec<u8>, ProtocolError> {
        borsh::to_vec(self).map_err(|error| ProtocolError::Serialization(error.to_string()))
    }

    pub(crate) fn validate(&self) -> Result<(), ProtocolError> {
        if self.domain != Self::DOMAIN || self.version != Self::VERSION {
            return Err(ProtocolError::InvalidSealData);
        }
        if self.proof_id == ProofId::from_bytes([0; 32])
            || self.receipt_id == ReceiptId::from_bytes([0; 32])
            || self.producer == PeerId([0; 32])
        {
            return Err(ProtocolError::InvalidSealData);
        }
        Ok(())
    }
}

/// Typed request for the local producer seal, emitted only after the complete
/// receipt body has been validated against the terminal certificate.
#[derive(
    BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
pub struct ProducerSealRequest {
    data: ReceiptSealData,
}

impl ProducerSealRequest {
    /// Construct a request for one complete receipt proof.
    #[must_use]
    pub(crate) const fn new(proof_id: ProofId, receipt_id: ReceiptId, producer: PeerId) -> Self {
        Self {
            data: ReceiptSealData::new(proof_id, receipt_id, producer),
        }
    }

    /// Return the proof body identity requested from the producer.
    #[must_use]
    pub const fn proof_id(&self) -> ProofId {
        self.data.proof_id()
    }

    /// Return the producer that must provide the Ed25519 seal.
    #[must_use]
    pub const fn producer(&self) -> PeerId {
        self.data.producer()
    }

    /// Borrow the typed seal preimage.
    #[must_use]
    pub const fn data(&self) -> &ReceiptSealData {
        &self.data
    }

    /// Return canonical bytes that the producer must sign.
    pub fn signing_bytes(&self) -> Result<Vec<u8>, ProtocolError> {
        self.data.signing_bytes()
    }
}

/// A producer's Ed25519 seal over one exact complete receipt body.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ProducerSeal {
    data: ReceiptSealData,
    signature: Ed25519Signature,
}

impl ProducerSeal {
    /// Construct a seal result returned by the typed producer worker.
    #[must_use]
    pub const fn new(data: ReceiptSealData, signature: Ed25519Signature) -> Self {
        Self { data, signature }
    }

    /// Borrow the exact request identity/preimage that was signed.
    #[must_use]
    pub const fn data(&self) -> &ReceiptSealData {
        &self.data
    }

    /// Return the Ed25519 signature bytes.
    #[must_use]
    pub const fn signature(&self) -> Ed25519Signature {
        self.signature
    }
}

/// Identity of a complete receipt body plus its producer seal.
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
pub struct ReceiptId([u8; 32]);

impl ReceiptId {
    /// Derive the public artifact identity before its producer seal is added.
    pub fn derive_body(body: &ReceiptBody) -> Result<Self, ProtocolError> {
        let bytes =
            borsh::to_vec(body).map_err(|error| ProtocolError::Serialization(error.to_string()))?;
        ensure_payload("receipt body", bytes.len(), MAX_RECEIPT_BYTES)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"arena0/receipt-body/v1");
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

/// The sole named public receipt artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    body: ReceiptBody,
    seal: ProducerSeal,
}

#[derive(Serialize)]
struct ReceiptSerialize<'a> {
    body: &'a ReceiptBody,
    seal: &'a ProducerSeal,
}

#[derive(Deserialize)]
struct ReceiptDeserialize {
    body: ReceiptBody,
    seal: ProducerSeal,
}

#[derive(BorshSerialize, BorshDeserialize)]
struct ReceiptWire {
    version: u8,
    body: ReceiptBody,
    seal: ProducerSeal,
}

impl Serialize for Receipt {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ReceiptSerialize {
            body: &self.body,
            seal: &self.seal,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Receipt {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = ReceiptDeserialize::deserialize(deserializer)?;
        Self::new(raw.body, raw.seal).map_err(serde::de::Error::custom)
    }
}

impl Receipt {
    /// Validate and assemble one final receipt artifact.
    pub fn new(body: ReceiptBody, seal: ProducerSeal) -> Result<Self, ProtocolError> {
        seal.data().validate()?;
        if body.header().producer != seal.data().producer() {
            return Err(ProtocolError::InvalidProducerSeal);
        }
        let proof_id = ProofId::derive(&body)?;
        let receipt_id = ReceiptId::derive_body(&body)?;
        if proof_id != seal.data().proof_id() || receipt_id != seal.data().receipt_id() {
            return Err(ProtocolError::InvalidProducerSeal);
        }
        let signing_bytes = seal.data().signing_bytes()?;
        let verified = arena0_crypto::verify(
            SignScheme::Ed25519,
            &seal.data().producer().0,
            &signing_bytes,
            &seal.signature().0,
        )
        .map_err(|error| ProtocolError::InvalidProducerSealCrypto(error.to_string()))?;
        if !verified {
            return Err(ProtocolError::InvalidProducerSeal);
        }
        let binding = super::ExecutionBinding::new(body.header().activation.clone())?;
        super::validate_receipt_body(&binding, body.header().producer, &body)?;
        let receipt = Self { body, seal };
        let encoded = borsh::to_vec(&receipt)
            .map_err(|error| ProtocolError::Serialization(error.to_string()))?;
        ensure_payload("receipt", encoded.len(), MAX_RECEIPT_BYTES)?;
        Ok(receipt)
    }

    /// Borrow the validated assembly body.
    #[must_use]
    pub const fn body(&self) -> &ReceiptBody {
        &self.body
    }

    /// Borrow the producer seal data and signature.
    #[must_use]
    pub const fn seal(&self) -> &ProducerSeal {
        &self.seal
    }

    /// Return the content identity of the complete body.
    #[must_use]
    pub const fn proof_id(&self) -> ProofId {
        self.seal.data().proof_id()
    }

    /// Return the public receipt identity.
    #[must_use]
    pub const fn receipt_id(&self) -> ReceiptId {
        self.seal.data().receipt_id()
    }

    /// Return the producer identity covered by the seal.
    #[must_use]
    pub const fn producer(&self) -> PeerId {
        self.seal.data().producer()
    }

    /// Return the exact protocol lookup key for this receipt.
    #[must_use]
    pub fn key(&self) -> ReceiptKey {
        self.body.key()
    }

    /// Encode the final artifact under the receipt bound.
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        let bytes =
            borsh::to_vec(self).map_err(|error| ProtocolError::Serialization(error.to_string()))?;
        ensure_payload("receipt", bytes.len(), MAX_RECEIPT_BYTES)?;
        Ok(bytes)
    }

    /// Decode and validate one final artifact.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        ensure_payload("receipt", bytes.len(), MAX_RECEIPT_BYTES)?;
        let wire: ReceiptWire = borsh::from_slice(bytes)
            .map_err(|error| ProtocolError::Deserialization(error.to_string()))?;
        if wire.version != RECEIPT_VERSION {
            return Err(ProtocolError::Deserialization(format!(
                "unknown receipt version {}",
                wire.version
            )));
        }
        Self::new(wire.body, wire.seal)
    }
}

impl BorshSerialize for Receipt {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        BorshSerialize::serialize(&RECEIPT_VERSION, writer)?;
        BorshSerialize::serialize(&self.body, writer)?;
        BorshSerialize::serialize(&self.seal, writer)
    }
}

impl BorshDeserialize for Receipt {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        let version = u8::deserialize_reader(reader)?;
        if version != RECEIPT_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown receipt version {version}"),
            ));
        }
        let body = ReceiptBody::deserialize_reader(reader)?;
        let seal = ProducerSeal::deserialize_reader(reader)?;
        Self::new(body, seal)
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
