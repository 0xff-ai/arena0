//! Authenticated inbound frames and durable inbox resolution.
//!
//! The transport reader only authenticates and queues a frame. This module
//! first records that responsibility in the inbox, then resolves messages
//! through the actor's flat dispatch or resolves protocol signatures/stops
//! in actor-owned state before persisting their transition records.

use arena0_protocol::{ExecFrame, ParticipantStepSignature};
use arena0_store::{Change, InboxAcceptOutcome, PendingInboxItem, StoreError};
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

    pub(super) async fn resolve_inbox_item(
        &mut self,
        item: PendingInboxItem,
    ) -> Result<(), ExecError> {
        match item.frame().clone() {
            frame @ ExecFrame::Message { .. } => {
                // A future message or a proposal-frozen message remains in
                // the inbox. apply_message rejects only invalid frames;
                // divergence reaches the actor's authenticated failure path.
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
        let state = &self.state;
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
        if proposal
            .signatures()
            .iter()
            .any(|existing| existing == &participant)
        {
            self.reject_inbound(item.inbox_id()).await?;
            return Ok(());
        }
        let mut next = self.state.clone();
        let certified = next.add_step_signature(participant)?;
        let agreed_step = certified.as_ref().map(|proposal| proposal.entry().step);
        self.persist(
            next,
            Change::StepSignature {
                certified,
                inbox_id: Some(item.inbox_id()),
            },
        )
        .await?;
        self.reconcile_resident()?;
        self.emit_trace_appended(agreed_step).await;
        Ok(())
    }

    async fn resolve_abort(
        &mut self,
        item: PendingInboxItem,
        occurrence: arena0_protocol::AbortOccurrence,
    ) -> Result<(), ExecError> {
        let state = &self.state;
        if state.status().is_terminal() {
            self.reject_inbound(item.inbox_id()).await?;
            return Ok(());
        }
        let mut next = self.state.clone();
        if next.stop(occurrence).is_err() {
            self.reject_inbound(item.inbox_id()).await?;
            return Ok(());
        }
        self.persist(
            next,
            Change::Stop {
                inbox_id: Some(item.inbox_id()),
            },
        )
        .await?;
        Ok(())
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
