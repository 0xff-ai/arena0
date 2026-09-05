//! Execution-instance identity.

use serde::{Deserialize, Serialize};

use crate::id::id_type;

id_type!(
    /// A 32-byte execution identity, assigned when a program is loaded for execution.
    pub struct Id
);

/// The observable lifecycle of an execution, from negotiation through terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, valuable::Valuable)]
pub enum ExecLifecycle {
    /// Negotiation is queued or running.
    Negotiating,
    /// The committed activation is proposed and participants are ratifying it.
    Activating,
    /// Execution is waiting for a private continuation answer or timer resume.
    Waiting,
    /// The session was confirmed and started; execution is running.
    Active,
    /// Execution completed successfully.
    Completed,
    /// The session aborted.
    Aborted,
    /// Terminal evidence was interrupted before a receipt could be published.
    /// Collected terminal proof remains durable, but no receipt exists.
    Incomplete,
    /// Negotiation or execution failed.
    Failed,
}

impl ExecLifecycle {
    /// Whether this lifecycle is terminal and cannot make further progress.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Aborted | Self::Incomplete | Self::Failed
        )
    }
}
