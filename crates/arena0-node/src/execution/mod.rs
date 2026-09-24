//! Durable execution actors.
//!
//! One private [`ExecutionActor`] owns the live guest and transport handles
//! for an execution. The SQLite [`arena0_store::ExecutionStore`] remains the
//! persistence boundary for actor-computed state; the sibling modules below organize
//! the actor by lifecycle and effect boundary.

use std::collections::HashMap;
use std::time::Duration;

use arena0_sandbox::ProgramInstance;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::context::{ActorContext, SessionMessage};
use crate::unix_time_ms as now_ms;

mod actor;
mod delivery;
mod guest;
mod terminal;

pub(crate) use actor::spawn_execution;
pub(crate) use terminal::fail_execution;

const COMMAND_CAPACITY: usize = 64;
const STREAM_CAPACITY: usize = 64;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);
const MAX_TIMER_BATCH: usize = 16;

/// Sole live owner of one loaded guest and its external capabilities.
///
/// The actor is intentionally private: callers interact through the
/// command and observation handles created by `spawn_execution`.
struct ExecutionActor {
    context: ActorContext,
    state: arena0_protocol::ExecutionState,
    /// The sole live Wasm instance for this execution.  The compiled
    /// `LoadedProgram` stays in `ActorContext` and may be shared by other
    /// actors, but this instance is execution-local and is only entered by
    /// the serialized actor loop.
    pub(super) instance: Option<ProgramInstance>,
    messages: mpsc::Sender<SessionMessage>,
    send_lanes: HashMap<arena0_protocol::PeerId, delivery::SendLane>,
    send_tasks: JoinSet<delivery::SendResult>,
    end_deadline: tokio::time::Instant,
    /// Whether this actor lifetime has delivered the durable session-start
    /// handoff to its observer. A restart may intentionally deliver it again;
    /// the durable public boundary remains the source of truth.
    session_started_emitted: bool,
    /// Whether this actor lifetime has delivered the durable terminal
    /// publication and lifecycle observation. Recovery intentionally emits
    /// them again for a new observer; ticker progress must not duplicate them
    /// within one actor lifetime.
    terminal_emitted: bool,
    /// Last open-callout identity announced to the observer. A restart starts
    /// empty so the committed callout is announced once again.
    announced_callout: Option<arena0_protocol::PendingId>,
}

fn callout_requested(
    pending_id: arena0_protocol::PendingId,
    callout_index: u32,
    context: Vec<u8>,
) -> SessionMessage {
    SessionMessage::CalloutRequested {
        pending_id,
        callout_index,
        context,
    }
}

/// Bound a human-readable reason to `limit` UTF-8 bytes at a scalar boundary.
pub(crate) fn truncate_reason(mut reason: String, limit: usize) -> String {
    while reason.len() > limit {
        // Popping whole scalar values keeps the cut at a UTF-8 boundary.
        // Calling `String::truncate` at the byte limit directly would panic
        // when the limit falls in the middle of a multibyte character.
        let _ = reason.pop();
    }
    reason
}

#[cfg(test)]
mod tests;
