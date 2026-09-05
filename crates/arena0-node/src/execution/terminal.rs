//! Terminal transitions, failure recovery, and receipt proof interruption.
//!
//! This module owns the durable abort and terminal-proof boundaries. Receipt
//! publication itself remains an outbox effect handled by `outbox`.

use crate::context::{ExecError, SessionMessage};
use arena0_crypto::NodeKeys;
use arena0_protocol::{
    AbortKind, AbortOccurrence, ExecutionInput, PeerIdSource, ReceiptSealData, ReceiptWork,
};
use arena0_store::{ApplyOutcome, ExecutionStore};

use super::{ExecutionActor, MAX_CAS_RETRIES, now_ms, truncate_reason};

impl ExecutionActor {
    pub(super) async fn terminate(&mut self, reason: String) -> Result<(), ExecError> {
        self.persist_abort(AbortKind::Abort, 0, reason).await?;
        self.progress().await
    }

    pub(super) async fn fail_terminal(&mut self, error: ExecError) {
        let reason = truncate_reason(error.to_string());
        match self.context.store.load_execution().await {
            Ok(Some(_)) => match fail_execution(
                &mut self.context.store,
                &self.context.identity,
                reason.clone(),
            )
            .await
            {
                Ok(outcome) => {
                    if let Ok(state) = self.load_state().await
                        && !matches!(state.status().receipt_work(), ReceiptWork::Incomplete)
                    {
                        let _ = self.drain_outbox().await;
                    }
                    if matches!(outcome, FailureOutcome::Recorded) {
                        let _ = self.messages.send(SessionMessage::Failed { reason }).await;
                    }
                }
                Err(error) => tracing::error!(
                    exec_id = %self.context.exec_id, %error,
                    "unable to durably record execution failure"
                ),
            },
            Ok(None) => {
                // Activation failures happen before the execution aggregate
                // exists. Preserve the failure on the durable admission root
                // before exposing it to the supervisor.
                match self
                    .context
                    .store
                    .record_execution_request_failure(reason.clone())
                    .await
                {
                    Ok(_) => {
                        let _ = self.messages.send(SessionMessage::Failed { reason }).await;
                    }
                    Err(record_error) => tracing::error!(
                        exec_id = %self.context.exec_id,
                        error = %record_error,
                        "unable to durably record pre-execution failure"
                    ),
                }
            }
            Err(load_error) => {
                tracing::error!(
                    exec_id = %self.context.exec_id,
                    error = %load_error,
                    "unable to load execution while recording failure"
                );
            }
        }
    }

    /// Commit a locally-authenticated abort/failure against the latest durable
    /// public head. The signature is derived from that exact head, and a CAS
    /// mismatch causes a fresh load and a fresh signed occurrence.
    async fn persist_abort(
        &mut self,
        kind: AbortKind,
        code: u32,
        reason: String,
    ) -> Result<bool, ExecError> {
        let reason = truncate_reason(reason);
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
                state.public(),
            )?;
            let signing_bytes = unsigned.signing_bytes()?;
            let occurrence = unsigned.with_signature(self.context.identity.sign(&signing_bytes))?;
            match self.apply_input(ExecutionInput::Abort(occurrence)).await? {
                ApplyOutcome::Committed(_) | ApplyOutcome::AlreadyApplied => return Ok(true),
                ApplyOutcome::VersionMismatch { .. } => continue,
                ApplyOutcome::Conflict(conflict) => {
                    return Err(ExecError::InvalidState(format!(
                        "abort conflicts with durable evidence: {conflict:?}"
                    )));
                }
                ApplyOutcome::InboxAlreadyApplied { .. }
                | ApplyOutcome::InboxAlreadyConsumed { .. } => {
                    return Err(ExecError::InvalidState(
                        "local abort unexpectedly carried inbox state".into(),
                    ));
                }
            }
        }
        Err(ExecError::Unavailable(
            "abort CAS retry limit exceeded".into(),
        ))
    }
}

/// Complete local proof work from durable evidence even when the guest or a
/// peer is unavailable. Outbox delivery retains its own causal order and may
/// redeliver the same producer-seal request after this idempotent handoff.
pub(crate) async fn finalize_receipt(
    store: &mut ExecutionStore,
    identity: &NodeKeys,
) -> Result<(), ExecError> {
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
            ReceiptWork::Assemble => match store.assemble_and_stage_receipt_body(now_ms()).await? {
                ApplyOutcome::Committed(_)
                | ApplyOutcome::AlreadyApplied
                | ApplyOutcome::VersionMismatch { .. } => {}
                ApplyOutcome::Conflict(conflict) => {
                    return Err(ExecError::InvalidState(format!(
                        "receipt assembly conflicts with durable proof: {conflict:?}"
                    )));
                }
                ApplyOutcome::InboxAlreadyApplied { .. }
                | ApplyOutcome::InboxAlreadyConsumed { .. } => {
                    return Err(ExecError::InvalidState(
                        "receipt assembly unexpectedly carried inbox state".into(),
                    ));
                }
            },
            ReceiptWork::Seal(request) => {
                return apply_producer_seal(store, identity, *request.data()).await;
            }
        }
    }
    Err(ExecError::Unavailable(
        "receipt finalization CAS retry limit exceeded".into(),
    ))
}

/// Whether a failure observation was committed or existing terminal evidence won.
pub(crate) enum FailureOutcome {
    Recorded,
    TerminalPreserved,
}

/// One failure transition for both a live actor and startup recovery. The Host
/// owns the signer; the execution store remains the sole durable writer.
pub(crate) async fn fail_execution(
    store: &mut ExecutionStore,
    identity: &NodeKeys,
    reason: String,
) -> Result<FailureOutcome, ExecError> {
    let reason = truncate_reason(reason);
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
        let input = match state.status().receipt_work() {
            ReceiptWork::Assemble | ReceiptWork::Seal(_) => {
                finalize_receipt(store, identity).await?;
                return Ok(FailureOutcome::TerminalPreserved);
            }
            ReceiptWork::Published | ReceiptWork::Incomplete => {
                return Ok(FailureOutcome::TerminalPreserved);
            }
            ReceiptWork::CollectSignatures => ExecutionInput::InterruptTerminal(reason.clone()),
            ReceiptWork::NotTerminal => {
                let unsigned = AbortOccurrence::unsigned(
                    state.binding().session_id(),
                    identity.peer_id(),
                    AbortKind::Fail,
                    1,
                    reason.clone(),
                    state.public(),
                )?;
                let signature = identity.sign(&unsigned.signing_bytes()?);
                ExecutionInput::Abort(unsigned.with_signature(signature)?)
            }
        };
        match store.apply_input(input, now_ms()).await? {
            ApplyOutcome::Committed(_) | ApplyOutcome::AlreadyApplied => {
                finalize_receipt(store, identity).await?;
                return Ok(FailureOutcome::Recorded);
            }
            ApplyOutcome::VersionMismatch { .. } => continue,
            ApplyOutcome::Conflict(conflict) => {
                return Err(ExecError::InvalidState(format!(
                    "failure conflicts with durable evidence: {conflict:?}"
                )));
            }
            ApplyOutcome::InboxAlreadyApplied { .. }
            | ApplyOutcome::InboxAlreadyConsumed { .. } => {
                return Err(ExecError::InvalidState(
                    "failure unexpectedly carried inbox state".into(),
                ));
            }
        }
    }
    Err(ExecError::Unavailable(
        "failure CAS retry limit exceeded".into(),
    ))
}

pub(super) async fn apply_producer_seal(
    store: &mut ExecutionStore,
    identity: &NodeKeys,
    data: ReceiptSealData,
) -> Result<(), ExecError> {
    if data.producer() != identity.peer_id() {
        return Err(ExecError::InvalidState(
            "producer seal names another Host".into(),
        ));
    }
    let seal = arena0_protocol::ProducerSeal::new(data, identity.sign(&data.signing_bytes()?));
    for _ in 0..MAX_CAS_RETRIES {
        match store
            .apply_input(ExecutionInput::ProducerSeal(seal.clone()), now_ms())
            .await?
        {
            ApplyOutcome::Committed(_) | ApplyOutcome::AlreadyApplied => return Ok(()),
            ApplyOutcome::VersionMismatch { .. } => continue,
            ApplyOutcome::Conflict(conflict) => {
                return Err(ExecError::InvalidState(format!(
                    "producer seal conflicts with durable proof: {conflict:?}"
                )));
            }
            ApplyOutcome::InboxAlreadyApplied { .. }
            | ApplyOutcome::InboxAlreadyConsumed { .. } => {
                return Err(ExecError::InvalidState(
                    "producer seal unexpectedly carried inbox state".into(),
                ));
            }
        }
    }
    Err(ExecError::Unavailable(
        "producer seal CAS retry limit exceeded".into(),
    ))
}
