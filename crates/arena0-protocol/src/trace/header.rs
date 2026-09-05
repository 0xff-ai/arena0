//! Session header: a self-describing label for a trace, naming the program,
//! session, and ensemble it claims to belong to.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::negotiation::Activation;
use arena0_crypto::BlsPublicKey;

use crate::{PeerId, SessionHash};
use arena0_program::ProgramHash;

use super::commitment::SessionTerminal;

/// Proof-bearing terminal evidence carried by a portable receipt header.
///
/// A receipt always names the way its execution ended.  Successful sessions
/// carry the full participant certificate for their terminal commitment;
/// stopped sessions carry the authenticated occurrence or the shared terminal
/// evidence that caused the stop.  Keeping these cases in one sum type avoids
/// the invalid state represented by an optional successful certificate on an
/// aborted receipt.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub enum ReceiptTermination {
    /// A `SessionEnd` certified by every committed participant.
    Completed { terminal: SessionTerminal },
    /// An authenticated abort/failure occurrence or certified shared stop.
    Stopped { cause: crate::execution::StopCause },
}

impl ReceiptTermination {
    /// Return the successful terminal evidence, if this receipt completed.
    #[must_use]
    pub const fn completed(&self) -> Option<&SessionTerminal> {
        match self {
            Self::Completed { terminal } => Some(terminal),
            Self::Stopped { .. } => None,
        }
    }

    /// Return the stopping evidence, if this receipt was stopped.
    #[must_use]
    pub const fn stopped(&self) -> Option<&crate::execution::StopCause> {
        match self {
            Self::Completed { .. } => None,
            Self::Stopped { cause } => Some(cause),
        }
    }
}

/// A self-describing label for a session trace: which program produced it, under
/// which session, and by which ensemble.
///
/// The header lets a holder of a trace blob know what it claims to be without
/// parsing every entry. It is NOT separately signed: its trustworthiness comes
/// from cross-checking its fields against the signed attestations inside the
/// trace (the verifier confirms the recovered `(program_hash, session_hash)` and
/// the ensemble match), so a forged header is caught at verification.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct SessionHeader {
    /// The confirmed session activation: the offer and its exact ticket set.
    pub activation: Activation,
    /// The proof-bearing terminal boundary. Completed receipts carry a signed
    /// `SessionTerminal`; stopped receipts carry authenticated or shared abort
    /// evidence. This field is intentionally non-optional: an artifact with no
    /// terminal evidence is not a receipt.
    pub terminal: ReceiptTermination,
}

impl SessionHeader {
    /// Build a header from the activation, proof-bearing terminal, and terminal evidence.
    #[must_use]
    pub fn new(activation: Activation, terminal: ReceiptTermination) -> Self {
        Self {
            activation,
            terminal,
        }
    }

    /// The content-addressed program the session ran.
    #[must_use]
    pub fn program_hash(&self) -> ProgramHash {
        self.activation.offer().data().program_hash
    }

    /// The session this trace belongs to.
    #[must_use]
    pub fn session_hash(&self) -> SessionHash {
        self.activation.session_hash()
    }

    /// Participant per-session BLS public keys in committed (participant) order, the
    /// order the signer bitmaps index into and the aggregates verify against.
    ///
    /// The committed participant order is the ensemble order: peers sorted by
    /// `PeerId`. The activation's tickets are in frozen (creator-first) order,
    /// so the keys are sorted here to match the bitmaps the execution records
    /// against the sorted ensemble.
    #[must_use]
    pub fn participant_keys(&self) -> Vec<BlsPublicKey> {
        let mut tickets = self.activation.tickets().to_vec();
        tickets.sort_by_key(|ticket| ticket.data.signer);
        tickets
            .iter()
            .map(|ticket| match &ticket.data.action {
                crate::TicketAction::Active { execution_bls, .. } => *execution_bls,
                crate::TicketAction::Withdrawn => unreachable!("activation tickets are Active"),
            })
            .collect()
    }

    /// The ensemble as `PeerId`s in committed (participant) order: sorted by
    /// `PeerId`, matching the order the signer bitmaps index into.
    #[must_use]
    pub fn ensemble(&self) -> Vec<PeerId> {
        let mut tickets = self.activation.tickets().to_vec();
        tickets.sort_by_key(|ticket| ticket.data.signer);
        tickets.iter().map(|ticket| ticket.data.signer).collect()
    }
}
