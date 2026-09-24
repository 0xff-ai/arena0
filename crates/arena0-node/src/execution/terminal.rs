//! Terminal transitions, failure recovery, and receipt publication.
//!
//! The actor computes stop and publication transitions. The store persists
//! complete records and assembles receipts from durable evidence.

use crate::context::{ExecError, SessionMessage};
use arena0_crypto::NodeKeys;
use arena0_protocol::{
    AbortKind, AbortOccurrence, ExecutionState, PeerIdSource, ReceiptTermination, ReceiptWork,
};
use arena0_store::{Change, ExecutionStore, TransitionRecord};

use super::{ExecutionActor, now_ms, truncate_reason};

impl ExecutionActor {
    pub(super) async fn terminate(&mut self, reason: String) -> Result<(), ExecError> {
        self.persist_abort(AbortKind::Abort, 0, reason).await?;
        self.progress().await
    }

    /// Record an actor failure and report whether the normal actor loop should
    /// stay alive to finish durable protocol delivery. Keeping that one loop
    /// alive is essential: it can accept a peer's terminal frame while our own
    /// `Abort` send is waiting for that peer's durable acknowledgement.
    pub(super) async fn fail_terminal(&mut self, error: ExecError) -> bool {
        let reason = truncate_reason(
            error.to_string(),
            arena0_protocol::MAX_TERMINAL_REASON_BYTES,
        );
        let preserved = matches!(self.state.status().receipt_work(), ReceiptWork::Assemble);
        let result = async {
            if matches!(self.state.status().receipt_work(), ReceiptWork::NotTerminal) {
                self.persist_abort(AbortKind::Fail, 1, reason).await?;
            }
            self.finalize_receipt().await
        }
        .await;
        match result {
            Ok(()) => true,
            Err(error) => {
                tracing::error!(exec_id = %self.context.exec_id, %error, "unable to durably record execution failure");
                preserved
            }
        }
    }

    /// Sign a stop over the actor's committed cursor and persist the result.
    async fn persist_abort(
        &mut self,
        kind: AbortKind,
        code: u32,
        reason: String,
    ) -> Result<bool, ExecError> {
        if self.state.status().is_terminal() {
            return Ok(false);
        }
        let next = stopped_state(&self.state, &self.context.identity, kind, code, reason)?;
        self.persist(next, Change::Stop).await?;
        Ok(true)
    }

    pub(super) async fn finalize_receipt(&mut self) -> Result<(), ExecError> {
        if !matches!(self.state.status().receipt_work(), ReceiptWork::Assemble) {
            return Ok(());
        }
        let artifact = self.context.store.assemble_receipt(&self.state).await?;
        let mut next = self.state.clone();
        next.publish_receipt(artifact.clone())?;
        self.persist(next, Change::Publish { artifact }).await
    }

    /// Deliver the observer-facing terminal boundary from its durable receipt.
    /// Publication is a local fact; the actor remains alive until every peer
    /// acknowledges the final protocol frames.
    pub(super) async fn emit_published_terminal(&mut self) -> Result<(), ExecError> {
        if self.terminal_emitted {
            return Ok(());
        }
        let state = &self.state;
        if !matches!(state.status().receipt_work(), ReceiptWork::Published) {
            return Ok(());
        }
        let receipt_id = state.published_receipt_id().ok_or_else(|| {
            ExecError::InvalidState("published terminal status has no receipt identity".into())
        })?;
        let stored = self
            .context
            .store
            .load_receipt_by_id(receipt_id)
            .await?
            .ok_or_else(|| {
                ExecError::InvalidState(
                    "published terminal status has no locally produced receipt".into(),
                )
            })?;
        if stored.receipt_id != receipt_id {
            return Err(ExecError::InvalidState(
                "published terminal status and local receipt identity differ".into(),
            ));
        }
        let receipt = stored.receipt;
        self.messages
            .send(SessionMessage::ReceiptPublished {
                receipt: receipt.clone(),
            })
            .await
            .map_err(|_| ExecError::Unavailable("message receiver closed".into()))?;
        let message = match receipt.body().termination() {
            ReceiptTermination::Completed => SessionMessage::Completed {
                result: receipt.body().outcome().to_vec(),
                result_json: state.terminal_outcome_json().map(ToOwned::to_owned),
            },
            ReceiptTermination::Stopped { cause } => {
                let step = match cause {
                    arena0_protocol::StopCause::Authenticated(occurrence) => {
                        occurrence.coordinate().next_step()
                    }
                    arena0_protocol::StopCause::Shared { commitment, .. } => commitment.step,
                };
                match cause.kind() {
                    AbortKind::Abort => SessionMessage::Aborted {
                        step,
                        reason: cause.reason().to_owned(),
                    },
                    AbortKind::Fail => SessionMessage::Failed {
                        reason: cause.reason().to_owned(),
                    },
                }
            }
        };
        self.messages
            .send(message)
            .await
            .map_err(|_| ExecError::Unavailable("message receiver closed".into()))?;
        self.terminal_emitted = true;
        Ok(())
    }
}

/// Whether failure was recorded or an existing terminal result was preserved.
pub(crate) enum FailureOutcome {
    Recorded,
    TerminalPreserved,
}

/// Compute the same authenticated stop for live actors and startup recovery.
fn stopped_state(
    state: &ExecutionState,
    identity: &NodeKeys,
    kind: AbortKind,
    code: u32,
    reason: String,
) -> Result<ExecutionState, ExecError> {
    if state.producer() != identity.peer_id() {
        return Err(ExecError::InvalidState(
            "failure signer is not the execution producer".into(),
        ));
    }
    let unsigned = AbortOccurrence::unsigned(
        state.binding().session_id(),
        identity.peer_id(),
        kind,
        code,
        truncate_reason(reason, arena0_protocol::MAX_TERMINAL_REASON_BYTES),
        state.step_cursor(),
    )?;
    let signature = identity.sign(&unsigned.signing_bytes()?);
    let occurrence = unsigned.with_signature(signature)?;
    let mut next = state.clone();
    next.stop(occurrence)?;
    Ok(next)
}

/// Record an execution failure during startup, before a live actor owns state.
/// The one loaded state advances only after each durable record succeeds.
pub(crate) async fn fail_execution(
    store: &mut ExecutionStore,
    identity: &NodeKeys,
    reason: String,
) -> Result<FailureOutcome, ExecError> {
    let mut state = store
        .load_execution()
        .await?
        .ok_or(ExecError::NotFound(store.execution_id()))?;
    let outcome = if matches!(state.status().receipt_work(), ReceiptWork::NotTerminal) {
        let next = stopped_state(&state, identity, AbortKind::Fail, 1, reason)?;
        store
            .persist(TransitionRecord {
                expected: state.version(),
                next: next.clone(),
                change: Change::Stop,
                now_ms: now_ms(),
            })
            .await?;
        state = next;
        FailureOutcome::Recorded
    } else {
        FailureOutcome::TerminalPreserved
    };
    if matches!(state.status().receipt_work(), ReceiptWork::Assemble) {
        let artifact = store.assemble_receipt(&state).await?;
        let expected = state.version();
        state.publish_receipt(artifact.clone())?;
        store
            .persist(TransitionRecord {
                expected,
                next: state,
                change: Change::Publish { artifact },
                now_ms: now_ms(),
            })
            .await?;
    }
    Ok(outcome)
}
