use arena0_crypto::BlsPublicKey;
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::SessionHash;
use crate::negotiation::Activation;
use crate::{Committed, Ensemble, EnsembleError, PeerId};
use arena0_program::{ExecutionProfileHash, ProgramHash};

use super::ProtocolError;
/// A durable binding to the exact activated session and deterministic program
/// environment. The validated activation is the sole owner of these values;
/// the accessors below derive the projections instead of storing redundant
/// copies that could drift.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ExecutionBinding {
    pub(crate) activation: Activation,
}

impl ExecutionBinding {
    /// Bind an execution to a validated activation.
    pub fn new(activation: Activation) -> Result<Self, ProtocolError> {
        activation
            .validate()
            .map_err(ProtocolError::InvalidActivation)?;
        Ok(Self { activation })
    }

    /// Return the exact validated activation.
    #[must_use]
    pub const fn activation(&self) -> &Activation {
        &self.activation
    }

    /// Return the activation's immutable session identity.
    #[must_use]
    pub fn session_id(&self) -> SessionHash {
        self.activation.session_hash()
    }

    /// Return the content-addressed program identity.
    #[must_use]
    pub const fn program_hash(&self) -> ProgramHash {
        self.activation.offer().data().program_hash
    }

    /// Return the complete deterministic execution-profile identity.
    #[must_use]
    pub const fn execution_profile(&self) -> ExecutionProfileHash {
        self.activation.offer().data().execution_profile
    }

    pub(crate) fn participant_keys(&self) -> Result<Vec<(PeerId, BlsPublicKey)>, ProtocolError> {
        let mut keys = self
            .activation
            .tickets()
            .iter()
            .filter_map(|ticket| match &ticket.data.action {
                crate::TicketAction::Active { execution_bls, .. } => {
                    Some((ticket.data.signer, *execution_bls))
                }
                crate::TicketAction::Withdrawn => None,
            })
            .collect::<Vec<_>>();
        keys.sort_by_key(|(peer, _)| *peer);
        if keys.len() != self.activation.tickets().len() {
            return Err(ProtocolError::BindingMismatch);
        }
        Ok(keys)
    }

    /// Participant execution keys in participant order, the order signer
    /// bitmaps index.
    pub(crate) fn participant_bls_keys(&self) -> Result<Vec<BlsPublicKey>, ProtocolError> {
        Ok(self
            .participant_keys()?
            .into_iter()
            .map(|(_, key)| key)
            .collect())
    }

    pub(crate) fn participant_key(
        &self,
        participant: &PeerId,
    ) -> Result<BlsPublicKey, ProtocolError> {
        // A direct scan: this runs once per signature and must not rebuild
        // and sort the whole key list.
        let ticket = self
            .activation
            .tickets()
            .iter()
            .find(|ticket| ticket.data.signer == *participant)
            .ok_or(ProtocolError::UnknownParticipant {
                participant: *participant,
            })?;
        match &ticket.data.action {
            crate::TicketAction::Active { execution_bls, .. } => Ok(*execution_bls),
            crate::TicketAction::Withdrawn => Err(ProtocolError::BindingMismatch),
        }
    }

    /// Participant identities in canonical activation order.
    pub fn participants(&self) -> impl Iterator<Item = PeerId> + '_ {
        self.activation
            .tickets()
            .iter()
            .map(|ticket| ticket.data.signer)
    }

    /// Whether `peer` holds a ticket in the activated ensemble.
    #[must_use]
    pub fn is_participant(&self, peer: PeerId) -> bool {
        self.participants().any(|participant| participant == peer)
    }

    /// The committed ensemble the guest observes, in `PeerId` order.
    pub fn ensemble(&self) -> Result<Ensemble<Committed>, EnsembleError> {
        Ensemble::from_peers(self.participants().collect())
    }
}
