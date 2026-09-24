//! Observer-facing session outcomes: completion, failure, or explicit stop.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

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

/// How a session ended, with its outcome or reported reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum SessionTermination {
    /// The program ended the session successfully.
    Completed {
        /// Borsh-encoded typed `Outcome`, projected from final shared state.
        outcome: Vec<u8>,
    },
    /// The runtime reported a failure, including a program failure or divergence.
    FailedHost { subsystem: String, message: String },
    /// An operator or the host explicitly stopped the session.
    Terminated { reason: String },
}

impl std::fmt::Display for SessionTermination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Completed { .. } => write!(f, "completed"),
            Self::FailedHost { message, .. } => write!(f, "{message}"),
            Self::Terminated { reason } => write!(f, "{reason}"),
        }
    }
}

impl SessionTermination {
    /// Returns `true` if the runtime reported a failure.
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
            SessionTermination::FailedHost {
                subsystem: "transport".into(),
                message: "connection reset".into(),
            },
            SessionTermination::Terminated {
                reason: "operator shutdown".into(),
            },
        ]
    }

    #[test]
    fn fault_predicates() {
        let variants = all_variants();
        assert_eq!(
            variants
                .iter()
                .map(|v| v.is_host_fault())
                .collect::<Vec<_>>(),
            vec![false, true, false]
        );
        assert_eq!(
            variants.iter().map(|v| v.is_success()).collect::<Vec<_>>(),
            vec![true, false, false]
        );
    }
}
