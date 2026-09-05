//! Authenticated inbound frames and durable inbox resolution.
//!
//! Transport readers only enqueue deliveries. This module performs the
//! durable acceptance, frame classification, and reducer application.

use arena0_protocol::ExecFrame;
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
            // A peer can present a frame that is well-formed on the wire but
            // is not authenticated for this activation. Reject that delivery
            // in place; an untrusted packet must not take the whole actor
            // terminal or poison the durable execution.
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
                // A transport acknowledgement is sent only after the frame
                // and its authenticated source have crossed the store
                // boundary.
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
                let _ = self
                    .apply_message(item.source(), frame, Some(item.inbox_id()))
                    .await?;
            }
            ExecFrame::StepSignature { .. } => {
                let state = self.load_state().await?;
                if state.pending_shared().is_none() {
                    // A participant can publish its signature as soon as it
                    // applies a proposal. Another participant may receive
                    // that signature before the proposal's message itself.
                    // The inbox acceptance above is already the durable
                    // responsibility boundary; retain the fact until the
                    // causal proposal is present instead of consuming a
                    // valid signature as stale.
                    if state.status().is_terminal() {
                        self.reject_inbound(item.inbox_id()).await?;
                    }
                    return Ok(());
                }
                let outcome = self
                    .context
                    .store
                    .apply_inbound(item.inbox_id(), now_ms())
                    .await;
                match outcome {
                    Ok(outcome) => {
                        self.emit_trace_appended(&outcome).await;
                        match outcome {
                            ApplyOutcome::VersionMismatch { .. } => Ok(()),
                            ApplyOutcome::Conflict(_) => self.reject_inbound(item.inbox_id()).await,
                            ApplyOutcome::InboxAlreadyApplied { .. }
                            | ApplyOutcome::InboxAlreadyConsumed { .. }
                            | ApplyOutcome::AlreadyApplied
                            | ApplyOutcome::Committed(_) => Ok(()),
                        }
                    }
                    Err(StoreError::InboxInputMismatch { .. }) => {
                        self.reject_inbound(item.inbox_id()).await
                    }
                    Err(error) => Err(error.into()),
                }?;
            }
            ExecFrame::End { .. } => {
                let state = self.load_state().await?;
                if !state.terminal_pending() {
                    // As with step signatures, terminal signatures may be
                    // reordered ahead of the shared terminal proposal. Keep
                    // the accepted fact until the terminal commitment is
                    // durable, where the store will validate its exact
                    // commitment and either apply or consume it.
                    if state.status().is_terminal() {
                        self.reject_inbound(item.inbox_id()).await?;
                    }
                    return Ok(());
                }
                let outcome = self
                    .context
                    .store
                    .apply_inbound(item.inbox_id(), now_ms())
                    .await;
                match outcome {
                    Ok(ApplyOutcome::VersionMismatch { .. }) => Ok(()),
                    Ok(ApplyOutcome::Conflict(_)) | Err(StoreError::InboxInputMismatch { .. }) => {
                        self.reject_inbound(item.inbox_id()).await
                    }
                    Ok(_) => Ok(()),
                    Err(error) => Err(error.into()),
                }?;
            }
            ExecFrame::Abort { .. } => {
                let outcome = self
                    .context
                    .store
                    .apply_inbound(item.inbox_id(), now_ms())
                    .await;
                match outcome {
                    Ok(ApplyOutcome::VersionMismatch { .. }) => Ok(()),
                    Ok(ApplyOutcome::Conflict(_)) | Err(StoreError::InboxInputMismatch { .. }) => {
                        self.reject_inbound(item.inbox_id()).await
                    }
                    Ok(_) => Ok(()),
                    Err(error) => Err(error.into()),
                }?;
            }
        }
        Ok(())
    }

    async fn reject_inbound(&mut self, inbox_id: arena0_store::InboxId) -> Result<(), ExecError> {
        self.context
            .store
            .reject_inbound(inbox_id, now_ms())
            .await?;
        Ok(())
    }
}
