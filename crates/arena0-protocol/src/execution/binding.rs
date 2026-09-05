use arena0_crypto::BlsPublicKey;
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::PeerId;
use crate::SessionHash;
use crate::negotiation::Activation;
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

    pub(crate) fn participant_key(
        &self,
        participant: &PeerId,
    ) -> Result<BlsPublicKey, ProtocolError> {
        self.participant_keys()?
            .into_iter()
            .find_map(|(peer, key)| (peer == *participant).then_some(key))
            .ok_or(ProtocolError::UnknownParticipant {
                participant: *participant,
            })
    }
}
