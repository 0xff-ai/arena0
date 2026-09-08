//! A confirmed session: the activation evidence bound to its participant
//! indexing authority.
//!
//! An [`ActivatedSession`] binds a cryptographically validated [`Activation`]
//! (the offer with the collective signature plus the exact ticket set) to the
//! committed participant ordering that execution and receipts use. Pure state;
//! no network, persistence, or time.

use arena0_protocol::{Activation, Committed, Ensemble, PeerId, SessionHash};
use thiserror::Error;

/// Why a confirmed activation could not form a session.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ActivatedSessionError {
    /// The activation's ticket set could not form a canonical ensemble.
    #[error("invalid activation ensemble: {0}")]
    InvalidEnsemble(String),
    /// The activation failed certificate-chain verification.
    #[error("activation verification failure: {0}")]
    AggregateVerification(String),
}

/// A ticket-negotiated session carrying the exact activation proof that
/// execution and receipts use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivatedSession {
    ensemble: Ensemble<Committed>,
    activation: Activation,
}

impl ActivatedSession {
    /// Bind an activation to its participant indexing authority. The full
    /// certificate chain is verified: every ticket's ed25519 identity
    /// signature, every Active ticket's scope-bound `key_binding`, and the
    /// collective signature over the exact `ActivationData`.
    pub fn new(activation: Activation) -> Result<Self, ActivatedSessionError> {
        activation
            .validate()
            .map_err(|error| ActivatedSessionError::AggregateVerification(error.to_string()))?;
        let peers = activation
            .tickets()
            .iter()
            .map(|ticket| ticket.data.signer)
            .collect::<Vec<PeerId>>();
        let ensemble = Ensemble::from_peers(peers)
            .map_err(|error| ActivatedSessionError::InvalidEnsemble(error.to_string()))?;
        Ok(Self {
            ensemble,
            activation,
        })
    }

    /// The committed participant ordering derived from the activation's exact
    /// ticket set.
    #[must_use]
    pub fn ensemble(&self) -> &Ensemble<Committed> {
        &self.ensemble
    }

    /// The cryptographically validated activation proof.
    #[must_use]
    pub fn activation(&self) -> &Activation {
        &self.activation
    }

    /// The session identity: BLAKE3 over the canonical `ActivationData`.
    #[must_use]
    pub fn session_hash(&self) -> SessionHash {
        self.activation.session_hash()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0_crypto::{BlsSignature, ExecutionKey, ExecutionSalt, NodeKeys, SecretKey};
    use arena0_program::ProgramHash;
    use arena0_protocol::{
        AggregateAttestation, ExecutionInput, ExecutionState, LocalStateBytes, NegotiationId,
        OfferData, OfferHash, PeerIdSource, PreparedActivation, PublicEvent, SharedDelta,
        SharedStateBytes, StateHash, TRACE_FORMAT_VERSION, Ticket, TicketAction, TicketData,
        TicketHash, TraceEntry, TransitionOutcome,
    };

    struct TestKeys {
        identity: NodeKeys,
        execution: ExecutionKey,
    }

    impl std::ops::Deref for TestKeys {
        type Target = NodeKeys;

        fn deref(&self) -> &Self::Target {
            &self.identity
        }
    }

    fn provider(seed: u8) -> TestKeys {
        TestKeys {
            identity: NodeKeys::from_secret(SecretKey::from_bytes([seed; 32])),
            execution: ExecutionKey::derive(
                &ExecutionSalt::try_from_bytes([seed; 32]).expect("non-zero test salt"),
                &[seed; 32],
                &[seed.wrapping_add(1); 32],
            )
            .expect("execution bls key"),
        }
    }

    fn ticket(p: &TestKeys, offer_data: &OfferData) -> Ticket {
        let execution_bls = p.execution.public_key();
        let key_binding = p.execution.key_binding(
            &OfferHash::of(offer_data).0,
            &p.identity.ed25519_public_key().0,
        );
        let data = TicketData::new(
            offer_data.negotiation_id,
            offer_data.offer_seq,
            p.peer_id(),
            0,
            TicketAction::Active {
                execution_bls,
                key_binding,
                issued_at_unix_ms: 1_000,
                valid_for_ms: 60_000,
            },
        )
        .expect("valid ticket data");
        Ticket {
            signature: p.sign(&data.signing_bytes()),
            data,
        }
    }

    /// A valid bilateral activation: creator `a` first, `b` ascending, with a
    /// real collective signature over the exact `ActivationData`.
    fn bilateral() -> (TestKeys, TestKeys, Activation) {
        let a = provider(1);
        let b = provider(2);
        let offer_data = OfferData::new(
            arena0_protocol::NegotiationId([0x11; 32]),
            0,
            a.peer_id(),
            ProgramHash([0x22; 32]),
            arena0_program::ExecutionProfile::current().hash(),
            arena0_program::JsonBytes::try_new(br#"{}"#.to_vec()).expect("valid JSON"),
            2,
            StateHash([0x33; 32]),
            1_000,
        )
        .expect("valid offer data");
        let ta = ticket(&a, &offer_data);
        let tb = ticket(&b, &offer_data);
        let tickets = vec![ta, tb];
        let hashes = tickets
            .iter()
            .map(|t| TicketHash::of(&t.data))
            .collect::<Vec<_>>();
        let offer = arena0_protocol::Offer::new(offer_data, hashes).expect("offer");
        let prepared = PreparedActivation::new(offer, tickets).expect("prepared activation");
        let sig_a = a
            .execution
            .sign(&prepared.activation_data().signing_bytes());
        let sig_b = b
            .execution
            .sign(&prepared.activation_data().signing_bytes());
        let aggregate = BlsSignature::aggregate(&[sig_a, sig_b]).expect("aggregate");
        let activation = Activation::new(prepared, aggregate).expect("valid activation");
        (a, b, activation)
    }

    #[test]
    fn valid_bilateral_session() {
        let (a, b, activation) = bilateral();
        let session = ActivatedSession::new(activation.clone()).expect("valid session");
        assert_eq!(session.ensemble().len(), 2);
        let mut expected = vec![a.peer_id(), b.peer_id()];
        expected.sort();
        assert_eq!(session.ensemble().peers(), &expected);
        assert_eq!(
            session.session_hash(),
            activation.session_hash(),
            "the session hash is the activation's derived SessionHash"
        );
    }

    #[test]
    fn participant_signatures_are_independent_of_activation_ticket_order() {
        // Activation tickets are creator-first while the committed ensemble
        // is sorted by PeerId, so execution keys are resolved by signer.
        let creator = provider(3);
        let mut others = [provider(1), provider(2)];
        others.sort_by_key(|p| p.peer_id());
        assert!(
            creator.peer_id() > others[0].peer_id(),
            "the creator must not be the smallest peer for the orders to diverge"
        );
        let offer_data = OfferData::new(
            NegotiationId([0x11; 32]),
            0,
            creator.peer_id(),
            ProgramHash([0x22; 32]),
            arena0_program::ExecutionProfile::current().hash(),
            arena0_program::JsonBytes::try_new(br#"{}"#.to_vec()).expect("valid JSON"),
            3,
            StateHash::of(&[0]),
            1_000,
        )
        .expect("valid offer data");
        let providers = [&creator, &others[0], &others[1]];
        let tickets = providers
            .iter()
            .map(|p| ticket(p, &offer_data))
            .collect::<Vec<_>>();
        let ticket_hashes = tickets
            .iter()
            .map(|ticket| TicketHash::of(&ticket.data))
            .collect::<Vec<_>>();
        let offer = arena0_protocol::Offer::new(offer_data, ticket_hashes).expect("offer");
        let prepared = PreparedActivation::new(offer, tickets).expect("prepared");
        let activation_data = prepared.activation_data();
        let sigs = providers
            .iter()
            .map(|p| p.execution.sign(&activation_data.signing_bytes()))
            .collect::<Vec<_>>();
        let aggregate = BlsSignature::aggregate(&sigs).expect("aggregate");
        let activation = Activation::new(prepared, aggregate).expect("valid activation");
        let initial = SharedStateBytes::try_new(vec![0]).expect("initial shared state");
        let state = ExecutionState::new(
            arena0_protocol::ExecId([0x44; 32]),
            activation,
            creator.peer_id(),
            initial.clone(),
            LocalStateBytes::try_new(Vec::new()).expect("initial local state"),
        )
        .expect("execution state");
        let active = match arena0_protocol::transition(&state, ExecutionInput::Activate)
            .expect("activate execution")
        {
            TransitionOutcome::Commit(plan) => plan.next_state().clone(),
            TransitionOutcome::AlreadyApplied => unreachable!("execution starts activating"),
        };
        let ensemble = Ensemble::from_peers(
            providers
                .iter()
                .map(|provider| provider.peer_id())
                .collect(),
        )
        .expect("committed ensemble");
        assert_ne!(
            ensemble.peers()[0],
            creator.peer_id(),
            "the sorted ensemble and creator-first tickets must diverge"
        );
        let delta = SharedDelta::new(
            TraceEntry {
                trace_version: TRACE_FORMAT_VERSION,
                step: 0,
                event: PublicEvent::SessionStarted { ensemble },
                effects: Vec::new(),
                pre_state: StateHash::of(initial.as_bytes()),
                post_state: StateHash::of(initial.as_bytes()),
                fuel_used: 0,
                witness: None,
                agreement: AggregateAttestation::empty(),
            },
            initial,
            None,
        )
        .expect("session-start proposal");
        let mut proposed =
            match arena0_protocol::transition(&active, ExecutionInput::ProposeShared(delta))
                .expect("propose session start")
            {
                TransitionOutcome::Commit(plan) => plan.next_state().clone(),
                TransitionOutcome::AlreadyApplied => unreachable!("proposal is new"),
            };
        let commitment = proposed
            .pending_shared()
            .expect("pending session-start proposal")
            .commitment()
            .clone();
        for provider in providers {
            proposed = match arena0_protocol::transition(
                &proposed,
                ExecutionInput::StepSignature(arena0_protocol::ParticipantStepSignature::new(
                    provider.peer_id(),
                    commitment.step,
                    provider.execution.sign(&commitment.signing_bytes()),
                )),
            )
            .expect("signatures resolve by participant identity")
            {
                TransitionOutcome::Commit(plan) => plan.next_state().clone(),
                TransitionOutcome::AlreadyApplied => unreachable!("signature is new"),
            };
        }
        assert!(proposed.pending_shared().is_none());
        assert_eq!(proposed.public().next_step(), 1);
    }

    #[test]
    fn tampered_aggregate_is_rejected() {
        let (_, _, activation) = bilateral();
        let mut aggregate = *activation.aggregate();
        aggregate.0[0] ^= 0xFF;
        assert!(matches!(
            Activation::new(activation.prepared().clone(), aggregate),
            Err(arena0_protocol::ActivationError::InvalidAttestations(_))
        ));
    }
}
