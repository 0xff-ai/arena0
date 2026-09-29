//! Shared valid execution setup for protocol behavior tests.

use crate::negotiation::{
    Offer, OfferData, OfferHash, PreparedActivation, Ticket, TicketData, TicketHash,
};
use crate::{Activation, NegotiationId, PeerId, StateHash, TicketAction};
use arena0_crypto::bls::BlsSecretKey;
use arena0_crypto::{BlsSignature, NodeKeys, SecretKey, key_binding_message};
use arena0_program::{ExecutionProfile, JsonBytes, ProgramHash, SharedStateBytes};

pub(crate) struct Fixture {
    pub(crate) activation: Activation,
    pub(crate) initial: SharedStateBytes,
    pub(crate) participants: Vec<(PeerId, BlsSecretKey)>,
}

impl Fixture {
    /// The Ed25519 identity behind `peer` (recomputed from the fixture's seeds).
    pub(crate) fn identity(&self, peer: PeerId) -> NodeKeys {
        [1, 2]
            .into_iter()
            .map(|seed| NodeKeys::from_secret(SecretKey::from_bytes([seed; 32])))
            .find(|identity| PeerId::from_ed25519(&identity.ed25519_public_key()) == peer)
            .expect("peer belongs to the fixture")
    }

    /// Bind the fixture's validated activation.
    pub(crate) fn binding(&self) -> super::ExecutionBinding {
        super::ExecutionBinding::new(self.activation.clone()).expect("fixture activation is valid")
    }

    pub(crate) fn producer(&self) -> PeerId {
        self.participants[0].0
    }
}

pub(crate) fn fixture() -> Fixture {
    fixture_with_initial(SharedStateBytes::try_new(vec![0x10, 0x20]).expect("state"))
}

pub(crate) fn fixture_with_initial(initial: SharedStateBytes) -> Fixture {
    let creator_identity = NodeKeys::from_secret(SecretKey::from_bytes([1; 32]));
    let other_identity = NodeKeys::from_secret(SecretKey::from_bytes([2; 32]));
    let mut identities = vec![
        (
            PeerId::from_ed25519(&creator_identity.ed25519_public_key()),
            creator_identity,
            BlsSecretKey::from_seed(&[11; 32]).expect("creator BLS key"),
        ),
        (
            PeerId::from_ed25519(&other_identity.ed25519_public_key()),
            other_identity,
            BlsSecretKey::from_seed(&[12; 32]).expect("peer BLS key"),
        ),
    ];
    identities.sort_by_key(|(peer, _, _)| *peer);

    let negotiation_id = NegotiationId([0x11; 32]);
    let offer_data = OfferData::new(
        negotiation_id,
        0,
        identities[0].0,
        ProgramHash([0x22; 32]),
        ExecutionProfile::current().hash(),
        JsonBytes::try_new(br#"{}"#.to_vec()).expect("valid params"),
        identities.len() as u16,
        StateHash::of_shared(&initial),
        1_000_000,
    )
    .expect("valid offer data");
    let offer_hash = OfferHash::of(&offer_data);
    let tickets = identities
        .iter()
        .map(|(peer, identity, bls)| {
            let execution_bls = bls.public_key();
            let key_binding =
                bls.sign_binding(&key_binding_message(&offer_hash.0, &peer.0, &execution_bls));
            let data = TicketData::new(
                negotiation_id,
                0,
                *peer,
                0,
                TicketAction::Active {
                    execution_bls,
                    key_binding,
                    issued_at_unix_ms: 1,
                    valid_for_ms: 60_000,
                },
            )
            .expect("valid ticket data");
            Ticket {
                signature: identity.sign(&data.signing_bytes()),
                data,
            }
        })
        .collect::<Vec<_>>();
    let ticket_hashes = tickets
        .iter()
        .map(|ticket| TicketHash::of(&ticket.data))
        .collect::<Vec<_>>();
    let offer = Offer::new(offer_data, ticket_hashes).expect("complete offer");
    let prepared = PreparedActivation::new(offer, tickets).expect("prepared activation");
    let activation_message = prepared.activation_data().signing_bytes();
    let activation_signatures = identities
        .iter()
        .map(|(_, _, bls)| bls.sign(&activation_message))
        .collect::<Vec<_>>();
    let activation = Activation::new(
        prepared,
        BlsSignature::aggregate(&activation_signatures).expect("activation aggregate"),
    )
    .expect("valid activation");
    let participants = identities
        .into_iter()
        .map(|(peer, _, bls)| (peer, bls))
        .collect();
    Fixture {
        activation,
        initial,
        participants,
    }
}
