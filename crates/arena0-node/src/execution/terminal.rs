//! Terminal transitions, failure recovery, and receipt publication.
//!
//! The actor signs only an authenticated abort occurrence over the current
//! agreed cursor. The store owns the stop/proof/publication transactions; no
//! terminal path re-enters the obsolete generic execution-input reducer.

use crate::context::{ExecError, SessionMessage};
use arena0_crypto::NodeKeys;
use arena0_protocol::{AbortKind, AbortOccurrence, PeerIdSource, ReceiptTermination, ReceiptWork};
use arena0_store::{ApplyOutcome, ExecutionStore};

use super::{ExecutionActor, MAX_CAS_RETRIES, now_ms, truncate_reason};

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
        match self.context.store.load_execution().await {
            Ok(Some(_)) => match fail_execution(
                &mut self.context.store,
                &self.context.identity,
                reason.clone(),
            )
            .await
            {
                Ok(outcome) => {
                    let incomplete = self.load_state().await.is_ok_and(|state| {
                        matches!(state.status().receipt_work(), ReceiptWork::Incomplete)
                    });
                    if matches!(outcome, FailureOutcome::Recorded) && incomplete {
                        let _ = self.messages.send(SessionMessage::Failed { reason }).await;
                    }
                    // Published and assembling terminals return to `run`, whose
                    // existing select loop owns inbound frames, outbox sends,
                    // durable retries, and the eventual terminal observation.
                    !incomplete
                }
                Err(error) => {
                    tracing::error!(
                        exec_id = %self.context.exec_id,
                        %error,
                        "unable to durably record execution failure"
                    );
                    false
                }
            },
            Ok(None) => match self
                .context
                .store
                .record_execution_request_failure(reason.clone())
                .await
            {
                Ok(_) => {
                    let _ = self.messages.send(SessionMessage::Failed { reason }).await;
                    false
                }
                Err(record_error) => {
                    tracing::error!(
                        exec_id = %self.context.exec_id,
                        error = %record_error,
                        "unable to durably record pre-execution failure"
                    );
                    false
                }
            },
            Err(load_error) => {
                tracing::error!(
                    exec_id = %self.context.exec_id,
                    error = %load_error,
                    "unable to load execution while recording failure"
                );
                false
            }
        }
    }

    /// Commit a locally authenticated abort/failure against the latest durable
    /// agreed cursor. A CAS mismatch causes a fresh load and a fresh signed
    /// occurrence, so a signature never covers a stale state head.
    async fn persist_abort(
        &mut self,
        kind: AbortKind,
        code: u32,
        reason: String,
    ) -> Result<bool, ExecError> {
        let reason = truncate_reason(reason, arena0_protocol::MAX_TERMINAL_REASON_BYTES);
        for _ in 0..MAX_CAS_RETRIES {
            let state = self.load_state().await?;
            if state.status().is_terminal() {
                return Ok(false);
            }
            let unsigned = AbortOccurrence::unsigned(
                state.binding().session_id(),
                self.context.identity.peer_id(),
                kind,
                code,
                reason.clone(),
                state.step_cursor(),
            )?;
            let signing_bytes = unsigned.signing_bytes()?;
            let occurrence = unsigned.with_signature(self.context.identity.sign(&signing_bytes))?;
            let outcome = self
                .context
                .store
                .stop_execution(state.version(), occurrence, None, now_ms())
                .await;
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(error) => {
                    self.restore_after_store_error().await;
                    return Err(error.into());
                }
            };
            match outcome {
                ApplyOutcome::Committed { .. }
                | ApplyOutcome::AlreadyApplied
                | ApplyOutcome::InboxAlreadyApplied { .. }
                | ApplyOutcome::InboxAlreadyConsumed { .. } => return Ok(true),
                ApplyOutcome::VersionMismatch { .. } => {
                    self.reload_resident().await?;
                }
            }
        }
        Err(ExecError::Unavailable(
            "abort compare-and-set retry limit exceeded".into(),
        ))
    }

    /// Deliver the observer-facing terminal boundary from its durable
    /// receipt. Receipt publication used to travel through a reducer-created
    /// outbox effect; the flat store operation persists it directly, so the
    /// actor now owns this notification seam explicitly.
    pub(super) async fn emit_published_terminal(&mut self) -> Result<(), ExecError> {
        if self.terminal_emitted {
            return Ok(());
        }
        let state = self.load_state().await?;
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
            ReceiptTermination::Completed { .. } => SessionMessage::Completed {
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

/// Complete local proof work from durable evidence. Publication is itself a
/// direct store operation and remains idempotent across retries.
pub(crate) async fn finalize_receipt(store: &mut ExecutionStore) -> Result<(), ExecError> {
    for _ in 0..MAX_CAS_RETRIES {
        let state = store
            .load_execution()
            .await?
            .ok_or(ExecError::NotFound(store.execution_id()))?;
        match state.status().receipt_work() {
            ReceiptWork::NotTerminal
            | ReceiptWork::CollectSignatures
            | ReceiptWork::Published
            | ReceiptWork::Incomplete => return Ok(()),
            ReceiptWork::Assemble => match store.publish_terminal(state.version(), now_ms()).await?
            {
                ApplyOutcome::Committed { .. }
                | ApplyOutcome::AlreadyApplied
                | ApplyOutcome::InboxAlreadyApplied { .. }
                | ApplyOutcome::InboxAlreadyConsumed { .. } => return Ok(()),
                ApplyOutcome::VersionMismatch { .. } => {}
            },
        }
    }
    Err(ExecError::Unavailable(
        "receipt publication compare-and-set retry limit exceeded".into(),
    ))
}

/// Whether a failure observation was committed or existing terminal evidence
/// won the race.
pub(crate) enum FailureOutcome {
    Recorded,
    TerminalPreserved,
}

/// Record one producer-authenticated failure for a live actor or startup
/// recovery. A terminal proof in progress cannot be replaced by an abort; it
/// is frozen as incomplete so its partial agreement remains inspectable.
pub(crate) async fn fail_execution(
    store: &mut ExecutionStore,
    identity: &NodeKeys,
    reason: String,
) -> Result<FailureOutcome, ExecError> {
    let reason = truncate_reason(reason, arena0_protocol::MAX_TERMINAL_REASON_BYTES);
    for _ in 0..MAX_CAS_RETRIES {
        let state = store
            .load_execution()
            .await?
            .ok_or(ExecError::NotFound(store.execution_id()))?;
        if state.producer() != identity.peer_id() {
            return Err(ExecError::InvalidState(
                "failure signer is not the execution producer".into(),
            ));
        }
        match state.status().receipt_work() {
            ReceiptWork::Assemble => {
                finalize_receipt(store).await?;
                return Ok(FailureOutcome::TerminalPreserved);
            }
            ReceiptWork::Published | ReceiptWork::Incomplete => {
                return Ok(FailureOutcome::TerminalPreserved);
            }
            ReceiptWork::CollectSignatures => {
                match store
                    .interrupt_terminal(state.version(), reason.clone(), now_ms())
                    .await?
                {
                    ApplyOutcome::Committed { .. } | ApplyOutcome::AlreadyApplied => {
                        return Ok(FailureOutcome::Recorded);
                    }
                    ApplyOutcome::InboxAlreadyApplied { .. }
                    | ApplyOutcome::InboxAlreadyConsumed { .. } => {
                        return Err(ExecError::InvalidState(
                            "local terminal interruption unexpectedly carried inbox state".into(),
                        ));
                    }
                    ApplyOutcome::VersionMismatch { .. } => continue,
                }
            }
            ReceiptWork::NotTerminal => {
                let unsigned = AbortOccurrence::unsigned(
                    state.binding().session_id(),
                    identity.peer_id(),
                    AbortKind::Fail,
                    1,
                    reason.clone(),
                    state.step_cursor(),
                )?;
                let signing_bytes = unsigned.signing_bytes()?;
                let occurrence = unsigned.with_signature(identity.sign(&signing_bytes))?;
                let outcome = store
                    .stop_execution(state.version(), occurrence, None, now_ms())
                    .await;
                let outcome = match outcome {
                    Ok(outcome) => outcome,
                    Err(error) => return Err(error.into()),
                };
                match outcome {
                    ApplyOutcome::Committed { .. }
                    | ApplyOutcome::AlreadyApplied
                    | ApplyOutcome::InboxAlreadyApplied { .. }
                    | ApplyOutcome::InboxAlreadyConsumed { .. } => {
                        finalize_receipt(store).await?;
                        return Ok(FailureOutcome::Recorded);
                    }
                    ApplyOutcome::VersionMismatch { .. } => continue,
                }
            }
        }
    }
    Err(ExecError::Unavailable(
        "failure compare-and-set retry limit exceeded".into(),
    ))
}
