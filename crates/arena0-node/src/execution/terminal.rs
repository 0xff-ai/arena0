//! Terminal transitions, failure recovery, and receipt publication.
//!
//! The actor computes stop and publication transitions. The store persists
//! complete records and assembles receipts from durable evidence.

use crate::context::{ExecError, SessionMessage};
use arena0_crypto::NodeKeys;
use arena0_protocol::{
    AbortKind, AbortOccurrence, ExecutionState, PeerIdSource, ReceiptTermination, ReceiptWork,
};
use arena0_store::{Change, ExecutionStore};

use super::actor::persist_transition;
use super::{ExecutionActor, truncate_reason};

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
        let preserved = matches!(self.state.status().receipt_work(), ReceiptWork::Assemble);
        let result = fail_and_publish(
            &mut self.context.store,
            &mut self.state,
            &self.context.identity,
            error.to_string(),
        )
        .await;
        match self.resync_on_error(result).await {
            Ok(()) => true,
            Err(error) => {
                tracing::error!(exec_id = %self.context.exec_id, %error, "unable to durably record execution failure");
                preserved
            }
        }
    }

    /// Stop with this Host's `Fail` because `signature` is a peer's valid
    /// signature over `conflicting`, a different commitment for the staged
    /// step (`ExecutionState::stop_on_conflict`), even if this Host already
    /// signed. The reason names the peer and the step. Persists the stopped
    /// state; the normal progress path then publishes the receipt.
    ///
    /// `Ok(false)` without any change when the protocol refuses the evidence
    /// (`ProtocolError::NoConflict`); the caller answers the frame `Rejected`.
    pub(super) async fn fail_on_conflict(
        &mut self,
        conflicting: arena0_protocol::StepCommitment,
        signature: arena0_protocol::ParticipantStepSignature,
    ) -> Result<bool, ExecError> {
        let peer = signature.participant();
        let step = conflicting.step;
        let occurrence = stop_occurrence(
            &self.state,
            &self.context.identity,
            AbortKind::Fail,
            0,
            truncate_reason(
                format!(
                    "diverged at step {step}: participant {peer} signed a different commitment"
                ),
                arena0_protocol::MAX_TERMINAL_REASON_BYTES,
            ),
        )?;
        let mut next = self.state.clone();
        match next.stop_on_conflict(occurrence, &conflicting, signature) {
            Ok(()) => {}
            Err(arena0_protocol::ProtocolError::NoConflict { .. }) => return Ok(false),
            Err(error) => return Err(error.into()),
        }
        self.persist(next, Change::State).await?;
        Ok(true)
    }

    /// Sign a stop over the actor's committed cursor and persist the result.
    async fn persist_abort(
        &mut self,
        kind: AbortKind,
        code: u32,
        reason: String,
    ) -> Result<(), ExecError> {
        if self.state.status().is_terminal() {
            return Ok(());
        }
        let next = stopped_state(&self.state, &self.context.identity, kind, code, reason)?;
        self.persist(next, Change::State).await
    }

    pub(super) async fn finalize_receipt(&mut self) -> Result<(), ExecError> {
        let result = publish_receipt(&mut self.context.store, &mut self.state).await;
        self.resync_on_error(result).await
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
            ReceiptTermination::Stopped { cause } => match cause.kind() {
                AbortKind::Abort => SessionMessage::Aborted {
                    step: cause.step(),
                    reason: cause.reason().to_owned(),
                },
                AbortKind::Fail => SessionMessage::Failed {
                    reason: cause.reason().to_owned(),
                },
            },
        };
        self.messages
            .send(message)
            .await
            .map_err(|_| ExecError::Unavailable("message receiver closed".into()))?;
        self.terminal_emitted = true;
        Ok(())
    }
}

/// Build one authenticated stop occurrence over the committed cursor.
fn stop_occurrence(
    state: &ExecutionState,
    identity: &NodeKeys,
    kind: AbortKind,
    code: u32,
    reason: String,
) -> Result<AbortOccurrence, ExecError> {
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
    Ok(unsigned.with_signature(signature)?)
}

/// Compute the same authenticated stop for live actors and startup recovery.
fn stopped_state(
    state: &ExecutionState,
    identity: &NodeKeys,
    kind: AbortKind,
    code: u32,
    reason: String,
) -> Result<ExecutionState, ExecError> {
    let occurrence = stop_occurrence(state, identity, kind, code, reason)?;
    let mut next = state.clone();
    next.stop(occurrence)?;
    Ok(next)
}

/// Stop a running execution with a local `Fail`, then publish its receipt.
/// Live actors and startup recovery share this one sequence; `state` advances
/// only after each durable write succeeds. A terminal execution keeps its
/// conclusion and only finishes publication.
async fn fail_and_publish(
    store: &mut ExecutionStore,
    state: &mut ExecutionState,
    identity: &NodeKeys,
    reason: String,
) -> Result<(), ExecError> {
    if matches!(state.status().receipt_work(), ReceiptWork::NotTerminal) {
        let next = stopped_state(state, identity, AbortKind::Fail, 1, reason)?;
        persist_transition(store, state, next, Change::State).await?;
    }
    publish_receipt(store, state).await
}

/// Assemble and persist the local receipt once terminal evidence is complete.
async fn publish_receipt(
    store: &mut ExecutionStore,
    state: &mut ExecutionState,
) -> Result<(), ExecError> {
    if !matches!(state.status().receipt_work(), ReceiptWork::Assemble) {
        return Ok(());
    }
    let artifact = store.assemble_receipt(state).await?;
    let mut next = state.clone();
    next.publish_receipt(artifact.clone())?;
    persist_transition(store, state, next, Change::Publish { artifact }).await
}

/// Record an execution failure during startup, before a live actor owns state.
pub(crate) async fn fail_execution(
    store: &mut ExecutionStore,
    identity: &NodeKeys,
    reason: String,
) -> Result<(), ExecError> {
    let mut state = store
        .load_execution()
        .await?
        .ok_or(ExecError::NotFound(store.execution_id()))?;
    fail_and_publish(store, &mut state, identity, reason).await
}
