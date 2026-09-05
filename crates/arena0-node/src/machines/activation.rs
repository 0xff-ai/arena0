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
        NegotiationId, OfferData, OfferHash, PeerIdSource, PreparedActivation, StateHash, Ticket,
        TicketAction, TicketData, TicketHash,
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
    fn by_signer_key_lookup_survives_creator_first_order() {
        // The ensemble is sorted by PeerId, but the activation's tickets are
        // in frozen (creator-first) order. When the creator is not the
        // smallest peer, the orders diverge: the execution's key lookup must
        // resolve each participant's ticket by signer. An index-based
        // lookup (`tickets[participant.index()]`) resolved the wrong key and
        // every peer signature failed verification at 32-node scale.
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
            StateHash([0x33; 32]),
            1_000,
        )
        .expect("valid offer data");
        let providers = [&creator, &others[0], &others[1]];
        let tickets = providers
            .iter()
            .map(|p| ticket(p, &offer_data))
            .collect::<Vec<_>>();
        let mut ordered_tickets = tickets;
        let creator_index = ordered_tickets
            .iter()
            .position(|ticket| ticket.data.signer == creator.peer_id())
            .expect("creator ticket");
        let creator_ticket = ordered_tickets.remove(creator_index);
        ordered_tickets.sort_by_key(|ticket| ticket.data.signer);
        let mut ordered_hashes = vec![TicketHash::of(&creator_ticket.data)];
        ordered_hashes.extend(
            ordered_tickets
                .iter()
                .map(|ticket| TicketHash::of(&ticket.data)),
        );
        ordered_tickets.insert(0, creator_ticket);
        let offer = arena0_protocol::Offer::new(offer_data, ordered_hashes).expect("offer");
        let prepared = PreparedActivation::new(offer, ordered_tickets).expect("prepared");
        let activation_data = prepared.activation_data();
        let sigs = providers
            .iter()
            .map(|p| p.execution.sign(&activation_data.signing_bytes()))
            .collect::<Vec<_>>();
        let aggregate = BlsSignature::aggregate(&sigs).expect("aggregate");
        let activation = Activation::new(prepared, aggregate).expect("valid activation");
        let session = ActivatedSession::new(activation).expect("valid session");
        let ensemble = session.ensemble().peers();
        // The orders diverge: the sorted ensemble's first participant is not
        // the creator.
        assert_ne!(ensemble[0], creator.peer_id());
        let keys_by_peer = providers
            .iter()
            .map(|p| (p.peer_id(), p.execution.public_key()))
            .collect::<std::collections::HashMap<_, _>>();
        let mut diverged = false;
        for peer in ensemble {
            let by_signer = session
                .activation()
                .tickets()
                .iter()
                .find(|t| t.data.signer == *peer)
                .map(|t| match &t.data.action {
                    TicketAction::Active { execution_bls, .. } => *execution_bls,
                    TicketAction::Withdrawn => unreachable!("activation tickets are Active"),
                })
                .expect("every ensemble participant has an Active ticket");
            assert_eq!(
                by_signer, keys_by_peer[peer],
                "the by-signer lookup resolves the participant's own key"
            );
            let index = session
                .ensemble()
                .participant_of(peer)
                .expect("ensemble participant")
                .index();
            let by_index = match &session.activation().tickets()[index].data.action {
                TicketAction::Active { execution_bls, .. } => *execution_bls,
                TicketAction::Withdrawn => unreachable!("activation tickets are Active"),
            };
            diverged |= by_signer != by_index;
        }
        assert!(
            diverged,
            "the sorted ensemble and the creator-first ticket order must diverge for this regression to be meaningful"
        );
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

    #[test]
    fn ticket_set_mismatch_is_rejected() {
        let (_, _, activation) = bilateral();
        // Swap in a ticket that is not in the offer's ticket-hash list.
        let outsider = ticket(&provider(9), activation.offer().data());
        let mut tickets = activation.tickets().to_vec();
        tickets[1] = outsider;
        assert!(matches!(
            PreparedActivation::new(activation.offer().clone(), tickets),
            Err(arena0_protocol::ActivationError::TicketSetMismatch)
        ));
    }
}
