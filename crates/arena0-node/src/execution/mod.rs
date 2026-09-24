//! Durable execution actors.
//!
//! One private [`ExecutionActor`] owns the live guest and transport handles
//! for an execution. The SQLite [`arena0_store::ExecutionStore`] remains the
//! sole owner of durable protocol state; the sibling modules below organize
//! the actor by lifecycle and effect boundary.

use std::collections::HashMap;
use std::time::Duration;

use arena0_sandbox::ProgramInstance;
use arena0_transport::SendHandle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::context::{ActorContext, SessionMessage};
use crate::unix_time_ms as now_ms;

mod actor;
mod guest;
mod inbox;
mod outbox;
mod terminal;

pub(crate) use actor::spawn_execution;
pub(crate) use terminal::fail_execution;

const COMMAND_CAPACITY: usize = 64;
const STREAM_CAPACITY: usize = 64;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);
const MAX_TIMER_BATCH: usize = 16;
const MAX_INBOX_BATCH: usize = 64;
const MAX_CAS_RETRIES: usize = 8;

/// Sole live owner of one loaded guest and its external capabilities.
///
/// The actor is intentionally private: callers interact through the
/// command and observation handles created by `spawn_execution`.
struct ExecutionActor {
    context: ActorContext,
    /// The sole live Wasm instance for this execution.  The compiled
    /// `LoadedProgram` stays in `ActorContext` and may be shared by other
    /// actors, but this instance is execution-local and is only entered by
    /// the serialized actor loop.
    pub(super) instance: Option<ProgramInstance>,
    messages: mpsc::Sender<SessionMessage>,
    send_streams: HashMap<arena0_protocol::PeerId, SendHandle>,
    /// One remote effect may be waiting for the receiver's durable
    /// responsibility acknowledgement.  It is kept outside the actor's
    /// serialized command future so an inbound frame from that receiver can
    /// still reach the actor and release the acknowledgement.
    inflight_send: Option<InflightSend>,
    /// Whether this actor lifetime has delivered the durable session-start
    /// handoff to its observer. A restart may intentionally deliver it again;
    /// the durable public boundary remains the source of truth.
    session_started_emitted: bool,
    /// Whether this actor lifetime has delivered the durable terminal
    /// publication and lifecycle observation. Recovery intentionally emits
    /// them again for a new observer; ticker progress must not duplicate them
    /// within one actor lifetime.
    terminal_emitted: bool,
}

/// A leased remote outbox effect whose transport acknowledgement is being
/// awaited concurrently with the actor command loop.
///
/// Dropping a Tokio join handle detaches its task.  Aborting in `Drop` keeps
/// the send owned by this actor; the durable outbox lease then remains for
/// recovery if the actor stops before the acknowledgement arrives.
pub(super) struct InflightSend {
    pub(super) destination: arena0_protocol::PeerId,
    pub(super) outbox_id: arena0_store::OutboxId,
    pub(super) lease_id: arena0_store::LeaseId,
    pub(super) task: Option<JoinHandle<Result<(), arena0_transport::TransportError>>>,
}

fn callout_requested(
    pending_id: arena0_protocol::PendingId,
    callout_index: u32,
    context: Vec<u8>,
    expected_type: Option<String>,
) -> SessionMessage {
    SessionMessage::CalloutRequested {
        pending_id,
        callout_index,
        context,
        expected_type,
    }
}

impl InflightSend {
    pub(super) async fn wait(
        &mut self,
    ) -> Result<Result<(), arena0_transport::TransportError>, tokio::task::JoinError> {
        self.task
            .as_mut()
            .expect("in-flight send task exists while selected")
            .await
    }
}

impl Drop for InflightSend {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
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
