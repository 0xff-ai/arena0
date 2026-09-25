use arena0_crypto::BlsSignature;
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use std::io;

use crate::negotiation::MAX_PARAMS_LEN;
use crate::trace::{AggregateAttestation, SessionHeader, StepCommitment, StepSig, TraceEntry};
use crate::{PeerId, SessionHash};
use arena0_program::ProgramHash;
use arena0_program::bounded;

use super::{
    ExecutionBinding, MAX_RECEIPT_BYTES, MAX_RECEIPT_TRACE_ENTRIES, MAX_TERMINAL_OUTCOME_BYTES,
    ProtocolError, SharedProposal, StepCertificate, ensure_payload, verify_full_agreement,
    verify_step_signature,
};

impl StepCertificate {
    /// Verify N-of-N evidence against its session binding, independently of
    /// whether the receiving participant has restored the staged proposal yet.
    pub fn verify(&self, binding: &ExecutionBinding) -> Result<(), ProtocolError> {
        if !self.commitment.is_bound_to(binding.session_id()) {
            return Err(ProtocolError::InvalidCertificate(
                "step certificate binding mismatch".into(),
            ));
        }
        verify_full_agreement(
            &self.agreement,
            &self.commitment,
            &binding.participant_bls_keys()?,
        )
    }

    /// Build an activation-bound N-of-N certificate for a shared proposal.
    ///
    /// The commitment is derived by the caller from the staged entry; this
    /// method only checks signatures against it and bundles the aggregate.
    pub fn from_signatures(
        binding: &ExecutionBinding,
        commitment: &StepCommitment,
        proposal: &SharedProposal,
    ) -> Result<Self, ProtocolError> {
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
            verify_step_signature(key, commitment, signature)?;
            signer_set.set(index);
            signatures.push(signature.signature.sig);
        }

        let agreement = AggregateAttestation::from_signatures(signer_set, &signatures)
            .map_err(|error| ProtocolError::InvalidCertificate(error.to_string()))?;
        let keys = participants.iter().map(|(_, key)| *key).collect::<Vec<_>>();
        verify_full_agreement(&agreement, commitment, &keys)?;

        Ok(Self {
            commitment: commitment.clone(),
            agreement,
        })
    }
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

const RECEIPT_BODY_VERSION: u8 = 5;
const RECEIPT_VERSION: u8 = 5;

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
        BorshSerialize::serialize(&RECEIPT_BODY_VERSION, writer)?;
        BorshSerialize::serialize(&self.header, writer)?;
        bounded::write_bytes::<MAX_TERMINAL_OUTCOME_BYTES>(&self.outcome, writer)?;
        bounded::write_bytes::<MAX_PARAMS_LEN>(&self.params, writer)?;
        bounded::write_vec::<MAX_RECEIPT_TRACE_ENTRIES, TraceEntry>(&self.trace, writer)
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
        let outcome = bounded::read_bytes::<MAX_TERMINAL_OUTCOME_BYTES>(reader)?;
        let params = bounded::read_bytes::<MAX_PARAMS_LEN>(reader)?;
        let trace = bounded::read_vec::<MAX_RECEIPT_TRACE_ENTRIES, TraceEntry>(reader)?;
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
        hasher.update(b"arena0/receipt/v5");
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

/// The portable facts established by an authenticated [`ReceiptArtifact`].
///
/// Verification authenticates the opaque Borsh outcome but does not run the
/// guest to produce its JSON projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptSummary {
    /// The content identity of the verified artifact.
    pub receipt_id: ReceiptId,
    /// The content-addressed program named by the activation.
    pub program_id: ProgramHash,
    /// The activation-derived session identity.
    pub session_id: SessionHash,
    /// The canonical participant order used by signer bitmaps.
    pub ensemble: Vec<PeerId>,
    /// Number of contiguous certified trace entries.
    pub steps: u64,
    /// The authenticated terminal classification and, for a stop, its cause.
    pub terminal: crate::ReceiptTermination,
    /// Opaque stock-Borsh outcome bytes, present exactly for a completion.
    pub outcome_borsh: Option<Vec<u8>>,
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
    /// Reserve the largest empty-trace artifact for this activation. The trace
    /// budget adds certified entries to this size, so even a maximal outcome
    /// or authenticated failure reason remains publishable at the boundary.
    /// These unsigned shapes are used only by the real encoders for sizing.
    pub(crate) fn reserved_overhead(binding: &ExecutionBinding) -> Result<u64, ProtocolError> {
        use super::{AbortKind, AbortOccurrence, StepCursor, StopCause};
        use crate::{ReceiptTermination, StepCommitment};

        let activation = binding.activation();
        let initial = activation.offer().data().initial_state;
        let reason = "x".repeat(super::MAX_TERMINAL_REASON_BYTES);
        let occurrence = AbortOccurrence::unsigned(
            binding.session_id(),
            activation.tickets()[0].data.signer,
            AbortKind::Fail,
            0,
            reason.clone(),
            StepCursor::new(0, initial, crate::CHAIN_START),
        )?;
        let shared = StopCause::Shared {
            kind: AbortKind::Fail,
            commitment: StepCommitment {
                domain: crate::STEP_COMMIT_DOMAIN,
                session_id: binding.session_id(),
                step: 0,
                entry_hash: [0; 32],
                pre_state: initial,
                post_state: initial,
                link: crate::CHAIN_START,
            },
            reason,
        };
        let mut largest = 0;
        for (terminal, outcome) in [
            (
                ReceiptTermination::Completed,
                vec![0; super::MAX_TERMINAL_OUTCOME_BYTES],
            ),
            (
                ReceiptTermination::Stopped {
                    cause: StopCause::Authenticated(occurrence),
                },
                vec![],
            ),
            (ReceiptTermination::Stopped { cause: shared }, vec![]),
        ] {
            let body = ReceiptBody {
                header: SessionHeader::new(activation.clone(), terminal),
                outcome,
                params: activation.offer().data().params.as_bytes().to_vec(),
                trace: Vec::new(),
            };
            let shape = Self::Receipt(Receipt {
                body,
                id: ReceiptId::from_bytes([0; 32]),
            });
            let size = borsh::object_length(&shape)
                .map_err(|error| ProtocolError::Serialization(error.to_string()))?;
            largest = largest.max(size as u64);
        }
        Ok(largest)
    }

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

    /// Project the portable facts this authenticated artifact establishes.
    ///
    /// Every `ReceiptArtifact` is authenticated at construction, so this is
    /// the complete receipt verification: it never loads Wasm or a sandbox.
    #[must_use]
    pub fn summary(&self) -> ReceiptSummary {
        let body = self.body();
        let activation = &body.header().activation;
        let mut ensemble = activation
            .tickets()
            .iter()
            .map(|ticket| ticket.data.signer)
            .collect::<Vec<_>>();
        ensemble.sort_unstable();
        let terminal = body.termination().clone();
        let outcome_borsh = matches!(terminal, crate::ReceiptTermination::Completed)
            .then(|| body.outcome().to_vec());
        ReceiptSummary {
            receipt_id: self.receipt_id(),
            program_id: activation.offer().data().program_hash,
            session_id: activation.session_hash(),
            ensemble,
            // The trace is bounded by `MAX_RECEIPT_TRACE_ENTRIES`.
            steps: body.trace().len() as u64,
            terminal,
            outcome_borsh,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Activation, AggregateAttestation, CHAIN_START, Committed, Ensemble, Offer, OfferData,
        PreparedActivation, ReceiptBody, SessionHeader, SignerSet, StateHash, StepCommitment,
        StepEvent, StepTerminal, TRACE_FORMAT_VERSION, Ticket, TicketAction, TicketData,
        TraceEntry,
    };
    use crate::{PeerId, ReceiptTermination};
    use arena0_crypto::{BlsSignature, ExecutionKey, ExecutionSalt, NodeKeys, SecretKey};
    use arena0_program::ExecutionProfile;

    fn fixture() -> Vec<u8> {
        fixture_with_binding(ProgramHash([0x44; 32]), StateHash([0x11; 32]), false)
            .expect("valid fixture")
    }

    fn fixture_with_binding(
        program_hash: ProgramHash,
        initial_state: StateHash,
        two_steps: bool,
    ) -> Result<Vec<u8>, crate::ProtocolError> {
        fixture_with_message_sender(program_hash, initial_state, two_steps, None)
    }

    fn fixture_with_message_sender(
        program_hash: ProgramHash,
        initial_state: StateHash,
        two_steps: bool,
        message_sender: Option<PeerId>,
    ) -> Result<Vec<u8>, crate::ProtocolError> {
        let negotiation = crate::NegotiationId([9; 32]);
        let keys = [
            NodeKeys::from_secret(SecretKey::from_bytes([1; 32])),
            NodeKeys::from_secret(SecretKey::from_bytes([2; 32])),
        ];
        let mut participants = keys
            .iter()
            .enumerate()
            .map(|(index, identity)| {
                let peer = PeerId(identity.ed25519_public_key().0);
                let execution = ExecutionKey::derive(
                    &ExecutionSalt::try_from_bytes([index as u8 + 10; 32])
                        .expect("non-zero test salt"),
                    &[index as u8 + 20; 32],
                    &negotiation.0,
                )
                .expect("fixture execution key");
                (peer, identity, execution)
            })
            .collect::<Vec<_>>();
        participants.sort_by_key(|(peer, _, _)| *peer);
        let creator = participants[0].0;
        let params = br#"{"seed":1}"#.to_vec();
        let profile = ExecutionProfile::current().hash();
        let data = OfferData::new(
            negotiation,
            0,
            creator,
            program_hash,
            profile,
            arena0_program::JsonBytes::try_new(params.clone()).expect("fixture JSON params"),
            2,
            initial_state,
            1,
        )
        .expect("fixture offer");
        let offer_hash = crate::OfferHash::of(&data);
        let mut tickets = Vec::new();
        for (peer, identity, execution) in &participants {
            let action = TicketAction::Active {
                execution_bls: execution.public_key(),
                key_binding: execution.key_binding(&offer_hash.0, &peer.0),
                issued_at_unix_ms: 0,
                valid_for_ms: 1,
            };
            let ticket_data =
                TicketData::new(negotiation, 0, *peer, 0, action).expect("fixture ticket data");
            let signature = identity.sign(&ticket_data.signing_bytes());
            tickets.push(Ticket {
                data: ticket_data,
                signature,
            });
        }
        let ticket_hashes = tickets
            .iter()
            .map(|ticket| crate::TicketHash::of(&ticket.data))
            .collect::<Vec<_>>();
        let offer = Offer::new(data, ticket_hashes).expect("fixture offer");
        let prepared = PreparedActivation::new(offer, tickets).expect("fixture preparation");
        let activation_signatures = participants
            .iter()
            .map(|(_, _, execution)| execution.sign(&prepared.activation_data().signing_bytes()))
            .collect::<Vec<_>>();
        let activation = Activation::new(
            prepared,
            BlsSignature::aggregate(&activation_signatures).expect("aggregate"),
        )
        .expect("fixture activation");
        let session = activation.session_hash();
        let ensemble = Ensemble::<Committed>::from_peers(
            participants.iter().map(|(peer, _, _)| *peer).collect(),
        )
        .expect("fixture ensemble");
        let outcome = vec![7, 8, 9];
        let first_post_state = StateHash::of(&[1]);
        let final_state = if two_steps {
            StateHash::of(&[2])
        } else {
            first_post_state
        };
        let mut entries = Vec::new();
        let mut first = TraceEntry {
            trace_version: TRACE_FORMAT_VERSION,
            step: 0,
            event: StepEvent::SessionStarted {
                ensemble: ensemble.clone(),
            },
            pre_state: initial_state,
            post_state: first_post_state,
            terminal: (!two_steps).then(|| StepTerminal::End {
                outcome: outcome.clone(),
            }),
            agreement: AggregateAttestation::empty(),
        };
        let first_commitment = StepCommitment::for_entry(session, &first, CHAIN_START);
        let step_signatures = participants
            .iter()
            .map(|(_, _, execution)| execution.sign(&first_commitment.signing_bytes()))
            .collect::<Vec<_>>();
        first.agreement = AggregateAttestation::from_signatures(
            SignerSet::full(participants.len()).expect("full signer set"),
            &step_signatures,
        )
        .expect("step aggregate");
        entries.push(first);
        if two_steps {
            let msg = vec![42];
            let sender = message_sender.unwrap_or(participants[1].0);
            let mut second = TraceEntry {
                trace_version: TRACE_FORMAT_VERSION,
                step: 1,
                event: StepEvent::Message {
                    from: sender,
                    data: msg,
                },
                pre_state: first_post_state,
                post_state: final_state,
                terminal: Some(StepTerminal::End {
                    outcome: outcome.clone(),
                }),
                agreement: AggregateAttestation::empty(),
            };
            let second_commitment =
                StepCommitment::for_entry(session, &second, first_commitment.link_hash());
            let step_signatures = participants
                .iter()
                .map(|(_, _, execution)| execution.sign(&second_commitment.signing_bytes()))
                .collect::<Vec<_>>();
            second.agreement = AggregateAttestation::from_signatures(
                SignerSet::full(participants.len()).expect("full signer set"),
                &step_signatures,
            )
            .expect("step aggregate");
            entries.push(second);
        }
        let body = ReceiptBody::new(
            SessionHeader::new(activation, ReceiptTermination::Completed),
            outcome,
            params,
            entries,
        )
        .expect("fixture body");

        crate::ReceiptArtifact::new(body)?.encode()
    }

    #[test]
    fn invalid_receipt_encodings_fail_closed() {
        let mut samples = vec![Vec::new(), vec![0xff], vec![1], vec![1, 99]];
        samples.push(vec![0u8; crate::MAX_RECEIPT_BYTES + 1]);
        for bytes in samples {
            assert!(ReceiptArtifact::decode(&bytes).is_err());
        }

        let original = fixture();
        let cases = [0, 1, original.len() / 2, original.len() - 1];
        for offset in cases {
            let mut bytes = original.clone();
            bytes[offset] ^= 1;
            assert!(ReceiptArtifact::decode(&bytes).is_err());
        }

        for end in 0..original.len() {
            assert!(
                ReceiptArtifact::decode(&original[..end]).is_err(),
                "invalid prefix at {end}"
            );
        }
        let mut unknown = original;
        unknown[0] = 0xff;
        assert!(ReceiptArtifact::decode(&unknown).is_err());
    }

    #[test]
    fn obsolete_receipt_and_body_versions_are_rejected() {
        let original = fixture();
        for (offset, version) in [(0, 2), (1, 2), (0, 3), (1, 3)] {
            let mut bytes = original.clone();
            bytes[offset] = version;
            assert!(
                ReceiptArtifact::decode(&bytes).is_err(),
                "obsolete version at {offset}"
            );
        }
    }

    #[test]
    fn valid_canonical_receipt_is_accepted() {
        let bytes = fixture();
        let summary = ReceiptArtifact::decode(&bytes)
            .expect("fixture verifies")
            .summary();
        assert_eq!(summary.steps, 1);
        assert_eq!(summary.terminal, ReceiptTermination::Completed);
        assert_eq!(summary.outcome_borsh, Some(vec![7, 8, 9]));
    }

    #[test]
    fn valid_multi_step_message_receipt_is_accepted() {
        let bytes = fixture_with_binding(ProgramHash([0x44; 32]), StateHash([0x11; 32]), true)
            .expect("valid multi-step fixture");
        let summary = ReceiptArtifact::decode(&bytes)
            .expect("multi-step fixture verifies")
            .summary();
        assert_eq!(summary.steps, 2);
        assert_eq!(summary.terminal, ReceiptTermination::Completed);
        assert_eq!(summary.outcome_borsh, Some(vec![7, 8, 9]));
    }

    #[test]
    fn message_receipt_rejects_sender_outside_ensemble() {
        let error = fixture_with_message_sender(
            ProgramHash([0x44; 32]),
            StateHash([0x11; 32]),
            true,
            Some(PeerId([0x77; 32])),
        )
        .expect_err("outsider message must not become authenticated evidence");
        assert!(matches!(
            error,
            crate::ProtocolError::UnknownParticipant { .. }
        ));
    }
}
