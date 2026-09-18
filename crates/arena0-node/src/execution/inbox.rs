//! Authenticated inbound frames and durable inbox resolution.
//!
//! The transport reader only authenticates and queues a frame. This module
//! first records that responsibility in the inbox, then resolves messages
//! through the actor's flat dispatch or resolves protocol signatures/stops
//! through their focused store operations. It never invokes a second generic
//! execution-input pipeline.

use arena0_protocol::{ExecFrame, ParticipantStepSignature, ParticipantTerminalSignature};
use arena0_store::{ApplyOutcome, InboxAcceptOutcome, PendingInboxItem, StoreError};
use arena0_transport::{ExecDelivery, ExecDeliveryRejection};

use super::{ExecutionActor, MAX_INBOX_BATCH, now_ms};
use crate::context::ExecError;

impl ExecutionActor {
    pub(super) async fn inbound(&mut self, delivery: ExecDelivery) -> Result<(), ExecError> {
        let source = delivery.source();
        let frame = delivery.frame().clone();
        let accepted = match self
            .context
            .store
            .accept_inbound(source, frame.clone(), now_ms())
            .await
        {
            Ok(outcome) => outcome,
            Err(StoreError::UnauthenticatedSource(_)) => {
                delivery.reject(ExecDeliveryRejection::Rejected)?;
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        match accepted {
            InboxAcceptOutcome::Conflict => {
                delivery.reject(ExecDeliveryRejection::Conflict)?;
                return Ok(());
            }
            InboxAcceptOutcome::Accepted
            | InboxAcceptOutcome::AlreadyAccepted
            | InboxAcceptOutcome::AlreadyApplied
            | InboxAcceptOutcome::AlreadyConsumed => {
                // Transport acknowledgement follows the durable inbox
                // acceptance boundary, not the later guest dispatch.
                delivery.acknowledge()?;
            }
        }

        let pending = self
            .context
            .store
            .list_pending_inbox(MAX_INBOX_BATCH)
            .await?;
        let Some(item) = pending
            .into_iter()
            .find(|item| item.source() == source && item.frame() == &frame)
        else {
            return self.progress().await;
        };
        self.resolve_inbox_item(item).await?;
        self.progress().await
    }

    pub(super) async fn resolve_pending_inbox(&mut self) -> Result<(), ExecError> {
        let items = self
            .context
            .store
            .list_pending_inbox(MAX_INBOX_BATCH)
            .await?;
        for item in items {
            self.resolve_inbox_item(item).await?;
        }
        Ok(())
    }

    async fn resolve_inbox_item(&mut self, item: PendingInboxItem) -> Result<(), ExecError> {
        match item.frame().clone() {
            frame @ ExecFrame::Message { .. } => {
                // A future message or a proposal-frozen message remains in
                // the inbox. apply_message rejects only an invalid message or
                // a guest rejection, and leaves these causal waits pending.
                let _ = self
                    .apply_message(item.source(), frame, Some(item.inbox_id()))
                    .await?;
            }
            ExecFrame::StepSignature {
                commitment,
                signature,
            } => {
                self.resolve_step_signature(item, commitment, signature)
                    .await?;
            }
            ExecFrame::End {
                commitment,
                signature,
            } => {
                self.resolve_terminal_signature(item, commitment, signature)
                    .await?;
            }
            ExecFrame::Abort { occurrence } => {
                self.resolve_abort(item, occurrence).await?;
            }
        }
        Ok(())
    }

    async fn resolve_step_signature(
        &mut self,
        item: PendingInboxItem,
        commitment: arena0_protocol::StepCommitment,
        signature: arena0_crypto::BlsSignature,
    ) -> Result<(), ExecError> {
        let state = self.load_state().await?;
        let Some(proposal) = state.pending_shared() else {
            if state.status().is_terminal() {
                self.reject_inbound(item.inbox_id()).await?;
            }
            // A signature may arrive before the corresponding proposal. Keep
            // the accepted frame until the proposal is visible locally.
            return Ok(());
        };

        if commitment.step != proposal.commitment().step {
            if proposal.commitment().step.checked_add(1) == Some(commitment.step) {
                // One participant can observe the next proposal before this
                // actor has completed its current one. Preserve causal order.
                return Ok(());
            }
            self.reject_inbound(item.inbox_id()).await?;
            return Ok(());
        }
        if commitment != *proposal.commitment() {
            self.reject_inbound(item.inbox_id()).await?;
            return Ok(());
        }

        let participant = ParticipantStepSignature::new(item.source(), commitment.step, signature);
        let outcome = self
            .context
            .store
            .commit_step_signature(
                state.version(),
                participant,
                Some(item.inbox_id()),
                now_ms(),
            )
            .await;
        let Some(outcome) = self
            .settle_inbox_store_result(item.inbox_id(), outcome)
            .await?
        else {
            return Ok(());
        };
        match outcome {
            ApplyOutcome::Committed { agreed_step, .. } => {
                self.reload_resident().await?;
                self.emit_trace_appended(agreed_step).await;
            }
            ApplyOutcome::VersionMismatch { .. } => {}
            ApplyOutcome::AlreadyApplied
            | ApplyOutcome::InboxAlreadyApplied { .. }
            | ApplyOutcome::InboxAlreadyConsumed { .. } => {
                self.reload_resident().await?;
            }
        }
        Ok(())
    }

    async fn resolve_terminal_signature(
        &mut self,
        item: PendingInboxItem,
        commitment: arena0_protocol::TerminalCommitment,
        signature: arena0_crypto::BlsSignature,
    ) -> Result<(), ExecError> {
        let state = self.load_state().await?;
        let Some(expected) = state.pending_terminal() else {
            if state.status().is_terminal() {
                self.reject_inbound(item.inbox_id()).await?;
            }
            return Ok(());
        };
        if expected != &commitment {
            self.reject_inbound(item.inbox_id()).await?;
            return Ok(());
        }
        let participant = ParticipantTerminalSignature::new(item.source(), signature);
        let outcome = self
            .context
            .store
            .commit_terminal_signature(
                state.version(),
                participant,
                Some(item.inbox_id()),
                now_ms(),
            )
            .await;
        self.settle_inbox_store_result(item.inbox_id(), outcome)
            .await?;
        Ok(())
    }

    async fn resolve_abort(
        &mut self,
        item: PendingInboxItem,
        occurrence: arena0_protocol::AbortOccurrence,
    ) -> Result<(), ExecError> {
        let state = self.load_state().await?;
        if state.status().is_terminal() {
            self.reject_inbound(item.inbox_id()).await?;
            return Ok(());
        }
        let outcome = self
            .context
            .store
            .stop_execution(state.version(), occurrence, Some(item.inbox_id()), now_ms())
            .await;
        let outcome = match outcome {
            // A transport-authenticated peer can still send an invalid or
            // no-longer-applicable abort. It is durable inbox input to reject,
            // not a local actor failure to feed back into another stop.
            Err(StoreError::Protocol(_)) => {
                self.reject_inbound(item.inbox_id()).await?;
                return Ok(());
            }
            outcome => outcome,
        };
        self.settle_inbox_store_result(item.inbox_id(), outcome)
            .await?;
        Ok(())
    }

    async fn settle_inbox_store_result(
        &mut self,
        inbox_id: arena0_store::InboxId,
        outcome: Result<ApplyOutcome, StoreError>,
    ) -> Result<Option<ApplyOutcome>, ExecError> {
        match outcome {
            Ok(outcome) => Ok(Some(outcome)),
            Err(StoreError::InboxInputMismatch { .. }) => {
                self.reject_inbound(inbox_id).await?;
                Ok(None)
            }
            Err(error) => {
                self.restore_after_store_error().await;
                Err(error.into())
            }
        }
    }

    pub(super) async fn reject_inbound(
        &mut self,
        inbox_id: arena0_store::InboxId,
    ) -> Result<(), ExecError> {
        self.context
            .store
            .reject_inbound(inbox_id, now_ms())
            .await?;
        Ok(())
    }
}
