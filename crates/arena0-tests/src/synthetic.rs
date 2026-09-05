//! Small signed protocol fixtures for receipt-verification tests.
//!
//! These helpers intentionally build protocol evidence, not sandbox instances:
//! the guest owns DTOs and the live tests use real Wasm, while this module only
//! supplies deterministic keys and canonical public trace entries for
//! protocol-only checks.

use arena0_crypto::{BlsSignature, NodeKeys, SecretKey};
use arena0_program::ProgramHash;
use arena0_protocol::{
    AbortKind, AbortOccurrence, Activation, AggregateAttestation, Ensemble, MessageId,
    NegotiationId, PeerIdSource, PublicCursor, PublicEffect, PublicEvent, ReceiptArtifact,
    ReceiptBody, ReceiptTermination, SessionHash, SessionHeader, SignerSet, StateHash,
    StepCommitment, TraceEntry, WitnessCommitment,
};

use crate::fixtures::activation_for;

#[derive(Debug)]
pub struct Synthetic {
    cryptos: Vec<NodeKeys>,
    activation: Activation,
    session_hash: SessionHash,
    initial: StateHash,
}

impl Synthetic {
    pub fn new(n: u8) -> Self {
        let negotiation_id = NegotiationId([0x11; 32]);
        let program_hash = ProgramHash([0x22; 32]);
        let initial = StateHash([0x33; 32]);

        let mut cryptos: Vec<NodeKeys> = (0..n)
            .map(|seed| NodeKeys::from_secret(SecretKey::from_bytes([seed + 1; 32])))
            .collect();
        cryptos.sort_by_key(NodeKeys::peer_id);
        let activation = activation_for(
            &cryptos,
            negotiation_id,
            program_hash,
            br#"{}"#.to_vec(),
            initial,
        );
        let session_hash = activation.session_hash();
        Self {
            cryptos,
            activation,
            session_hash,
            initial,
        }
    }

    pub fn peer(&self, idx: usize) -> arena0_protocol::PeerId {
        self.activation.tickets()[idx].data.signer
    }

    pub fn ensemble(&self) -> Ensemble {
        Ensemble::from_peers(
            self.activation
                .tickets()
                .iter()
                .map(|ticket| ticket.data.signer)
                .collect(),
        )
        .expect("ensemble")
    }

    /// Aggregate the given participants' BLS signatures over `msg` into an
    /// `AggregateAttestation` with their bits set.
    pub fn aggregate(&self, msg: &[u8], indices: &[usize]) -> AggregateAttestation {
        let mut signers = SignerSet::with_capacity(self.cryptos.len());
        let mut sigs = Vec::new();
        for &i in indices {
            sigs.push(crate::fixtures::execution_key(&self.cryptos[i]).sign(msg));
            signers.set(i);
        }
        AggregateAttestation {
            aggregate: BlsSignature::aggregate(&sigs).expect("aggregate"),
            signers,
        }
    }

    /// Construct a header for an explicit stopped receipt. Successful headers
    /// are assembled by the live execution actor and are not faked here.
    pub fn stopped_header(&self, cause: arena0_protocol::StopCause) -> SessionHeader {
        SessionHeader::new(
            self.activation.clone(),
            ReceiptTermination::Stopped { cause },
        )
    }

    /// Construct a stopped artifact from authenticated protocol evidence.
    pub fn stopped_receipt(
        &self,
        cause: arena0_protocol::StopCause,
        trace: Vec<TraceEntry>,
    ) -> ReceiptArtifact {
        let header = self.stopped_header(cause);
        let body =
            ReceiptBody::new(header, Vec::new(), br#"{}"#.to_vec(), trace).expect("receipt body");
        ReceiptArtifact::new(body).expect("valid stopped receipt")
    }

    /// Construct one signed public entry and return its next chain commitment.
    pub fn signed_entry(
        &self,
        step: u64,
        event: PublicEvent,
        effects: Vec<PublicEffect>,
        witness: Option<WitnessCommitment>,
        link: [u8; 32],
        signers: &[usize],
    ) -> (TraceEntry, StepCommitment) {
        let mut entry = TraceEntry {
            trace_version: arena0_protocol::TRACE_FORMAT_VERSION,
            step,
            event,
            effects,
            pre_state: self.initial,
            post_state: self.initial,
            fuel_used: 0,
            witness,
            agreement: AggregateAttestation::empty(),
        };
        let commitment = StepCommitment::for_entry(self.session_hash, &entry, link);
        entry.agreement = self.aggregate(&commitment.signing_bytes(), signers);
        (entry, commitment)
    }

    pub fn started_entry(&self, signers: &[usize]) -> (TraceEntry, StepCommitment) {
        self.signed_entry(
            0,
            PublicEvent::SessionStarted {
                ensemble: self.ensemble(),
            },
            Vec::new(),
            None,
            arena0_protocol::CHAIN_START,
            signers,
        )
    }

    /// A `MessageReceived` event with a self-consistent content address.
    pub fn message_event(
        &self,
        step: u64,
        from_idx: usize,
        data: Vec<u8>,
        witness: WitnessCommitment,
    ) -> PublicEvent {
        let from = self.peer(from_idx);
        let message_id =
            MessageId::derive(self.session_hash, from, step, self.initial, &data, witness);
        PublicEvent::MessageReceived {
            message_id,
            position: step,
            pre_state: self.initial,
            from,
            msg: data,
        }
    }

    /// Sign an authenticated stop at the initial public cursor. Useful for
    /// testing the terminal split without manufacturing a trace entry.
    pub fn authenticated_stop(
        &self,
        sender_idx: usize,
        kind: AbortKind,
        reason: &str,
    ) -> arena0_protocol::StopCause {
        let sender = self.cryptos[sender_idx].peer_id();
        let cursor = PublicCursor::new(0, self.initial, arena0_protocol::CHAIN_START);
        let unsigned =
            AbortOccurrence::unsigned(self.session_hash, sender, kind, 1, reason, cursor)
                .expect("abort occurrence");
        let signature =
            self.cryptos[sender_idx].sign(&unsigned.signing_bytes().expect("abort bytes"));
        let occurrence = unsigned
            .with_signature(signature)
            .expect("signed occurrence");
        arena0_protocol::StopCause::Authenticated(occurrence)
    }
}
