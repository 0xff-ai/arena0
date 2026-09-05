//! Terminal results on public event and API boundaries.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The result reported when an execution reaches a terminal state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TerminalResult {
    /// The session completed with an optional guest-produced JSON outcome.
    Completed { outcome: Option<Value> },
    /// The session aborted at `step`.
    Aborted { step: u64, reason: String },
    /// The execution failed before or without a session terminal.
    Failed { reason: String },
}
