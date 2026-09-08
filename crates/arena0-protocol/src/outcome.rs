//! Terminal session outcomes with structured fault attribution.
//!
//! [`SessionTermination`] distinguishes guest faults (program bugs), host faults
//! (infrastructure failures), and shared divergence so that the runtime, proofs,
//! and UI can attribute responsibility precisely.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::StateHash;
use crate::id::id_type;

id_type!(
    /// `blake3` of the borsh-encoded typed `Outcome` bytes. Binding this hash into
    /// the signed terminal ties the receipt to the completion aggregate, so a
    /// relabeled outcome is caught. Construct with [`Hash::of`].
    pub struct Hash,
    Default
);

impl Hash {
    /// Hash the outcome bytes (the borsh-encoded typed `Outcome`).
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }
}

/// How a session ended. Every variant carries enough context to attribute fault
/// (guest vs. host) and to render a meaningful proof or error message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum SessionTermination {
    /// The program ended the session successfully.
    Completed {
        /// Borsh-encoded typed `Outcome`, projected from final shared state.
        outcome: Vec<u8>,
    },
    /// The program called `fail` explicitly.
    FailedGuest { reason: String },
    /// The Wasm guest trapped (panic, stack overflow, etc.).
    TrappedGuest {
        trap_kind: TrapKind,
        message: String,
    },
    /// A host subsystem (transport, store, crypto) failed.
    FailedHost { subsystem: String, message: String },
    /// Post-dispatch state hashes diverged between participants.
    Diverged {
        /// The step at which divergence was detected.
        step: u64,
        /// Our post-state hash.
        local_hash: StateHash,
        /// The peer's post-state hash.
        remote_hash: StateHash,
    },
    /// An operator or the host explicitly stopped the session.
    Terminated { reason: String },
    /// Replay or state restore produced inconsistent results.
    ReplayDesynced { step: u64, message: String },
}

impl std::fmt::Display for SessionTermination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Completed { .. } => write!(f, "completed"),
            Self::FailedGuest { reason } => write!(f, "the program failed: {reason}"),
            Self::TrappedGuest { trap_kind, message } => {
                write!(f, "the program trapped ({trap_kind:?}): {message}")
            }
            Self::FailedHost { message, .. } => write!(f, "{message}"),
            Self::Diverged {
                step,
                local_hash,
                remote_hash,
            } => write!(
                f,
                "state divergence at step {step}: local {local_hash} vs peer {remote_hash}"
            ),
            Self::Terminated { reason } => write!(f, "{reason}"),
            Self::ReplayDesynced { step, message } => {
                write!(f, "replay desync at step {step}: {message}")
            }
        }
    }
}

/// Classification of a Wasm guest trap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum TrapKind {
    /// The program panicked (e.g., `unreachable` instruction, Rust panic).
    Panic,
    /// The Wasm call stack exceeded the configured depth.
    StackOverflow,
    /// The program violated the host ABI contract.
    AbiMisuse,
    /// A trap that does not fit any known category.
    Unknown,
}

impl SessionTermination {
    /// Returns `true` if the program itself caused the failure.
    #[must_use]
    pub fn is_guest_fault(&self) -> bool {
        matches!(self, Self::FailedGuest { .. } | Self::TrappedGuest { .. })
    }

    /// Returns `true` if a host subsystem caused the failure.
    #[must_use]
    pub fn is_host_fault(&self) -> bool {
        matches!(self, Self::FailedHost { .. })
    }

    /// Returns `true` if the session completed successfully.
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Completed { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_variants() -> Vec<SessionTermination> {
        vec![
            SessionTermination::Completed {
                outcome: vec![1, 2, 3],
            },
            SessionTermination::FailedGuest {
                reason: "bad input".into(),
            },
            SessionTermination::TrappedGuest {
                trap_kind: TrapKind::Panic,
                message: "index out of bounds".into(),
            },
            SessionTermination::FailedHost {
                subsystem: "transport".into(),
                message: "connection reset".into(),
            },
            SessionTermination::Diverged {
                step: 42,
                local_hash: StateHash([0xAA; 32]),
                remote_hash: StateHash([0xBB; 32]),
            },
            SessionTermination::Terminated {
                reason: "operator shutdown".into(),
            },
            SessionTermination::ReplayDesynced {
                step: 7,
                message: "state mismatch at step 7".into(),
            },
        ]
    }

    #[test]
    fn fault_predicates() {
        let variants = all_variants();
        assert_eq!(
            variants
                .iter()
                .map(|v| v.is_guest_fault())
                .collect::<Vec<_>>(),
            vec![false, true, true, false, false, false, false]
        );
        assert_eq!(
            variants
                .iter()
                .map(|v| v.is_host_fault())
                .collect::<Vec<_>>(),
            vec![false, false, false, true, false, false, false]
        );
        assert_eq!(
            variants.iter().map(|v| v.is_success()).collect::<Vec<_>>(),
            vec![true, false, false, false, false, false, false]
        );
    }
}
