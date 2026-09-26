//! Session header: a self-describing label for a trace, naming the program,
//! session, and ensemble it claims to belong to.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::SessionHash;
use crate::negotiation::Activation;
use arena0_program::ProgramHash;

/// Proof-bearing terminal evidence carried by a portable receipt header.
///
/// Completion is authenticated by the final certified SessionEnd trace entry.
/// Stops carry either a shared step commitment or an authenticated occurrence.
// Keep terminal causes inline, matching the execution status and verification result.
#[allow(clippy::large_enum_variant)]
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub enum ReceiptTermination {
    /// A `SessionEnd` certified by every committed participant.
    Completed,
    /// An authenticated abort/failure occurrence or certified shared stop.
    Stopped { cause: crate::execution::StopCause },
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
    /// The terminal classification and, for a stop, its authenticated cause.
    /// Completion evidence is retained in the final certified trace entry.
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
}
