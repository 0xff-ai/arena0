//! Durable effect leasing and external delivery.
//!
//! Outbox rows are acknowledged only after their external effect or local
//! reducer transition crosses its corresponding durable boundary.

use std::time::Instant;

use crate::context::{ExecError, SessionMessage};
use arena0_protocol::{DurableEffect, ExecFrame, PeerIdSource, PublicEvent};
use arena0_store::OutboxItem;
use arena0_transport::TransportError;

use super::{ExecutionActor, InflightSend, now_ms};

const PERFORMANCE_TARGET: &str = "arena0::performance";

#[derive(Debug, Clone, Copy)]
struct OutboxDrainSummary {
    count: usize,
    result_class: &'static str,
}

/// A leased effect can fail without invalidating the execution. Transport
/// outages belong to the durable retry loop; malformed durable effects,
/// guest failures, and closed observer channels are terminal actor failures.
#[derive(Debug)]
enum OutboxDeliveryError {
    Retryable(ExecError),
    Terminal(ExecError),
}

impl From<ExecError> for OutboxDeliveryError {
    fn from(error: ExecError) -> Self {
        Self::Terminal(error)
    }
}

impl OutboxDeliveryError {
    fn into_runtime(self) -> ExecError {
        match self {
            Self::Retryable(error) | Self::Terminal(error) => error,
        }
    }
}

impl std::fmt::Display for OutboxDeliveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Retryable(error) | Self::Terminal(error) => error.fmt(formatter),
        }
    }
}

fn classify_transport_error(error: TransportError) -> OutboxDeliveryError {
    let runtime = ExecError::from(error.clone());
    match error {
        TransportError::ExecRejected
        | TransportError::ExecConflict
        | TransportError::ProtocolMismatch(_)
        | TransportError::InvalidFrame(_)
        | TransportError::PayloadTooLarge { .. } => OutboxDeliveryError::Terminal(runtime),
        _ => OutboxDeliveryError::Retryable(runtime),
    }
}

impl ExecutionActor {
    pub(super) async fn drain_outbox(&mut self) -> Result<(), ExecError> {
        // An in-flight remote send is settled by its completion wakeup in the
        // actor loop. Timer and command-driven progress still runs while the
        // receiver is responsible for the durable acknowledgement, but must
        // not poll the join handle or issue a repeated no-op drain.
        if self.inflight_send.is_some() {
            return Ok(());
        }
        let started =
            tracing::enabled!(target: PERFORMANCE_TARGET, tracing::Level::DEBUG).then(Instant::now);
        let result = self.drain_outbox_inner().await;
        let has_work = match &result {
            Ok(summary) => summary.count != 0,
            Err(_) => true,
        };
        if has_work && let Some(started) = started {
            record_outbox_drain(
                started,
                self.context.exec_id,
                self.context.activation.session_hash(),
                &result,
            );
        }
        result.map(|_| ())
    }

    async fn drain_outbox_inner(&mut self) -> Result<OutboxDrainSummary, ExecError> {
        let mut count = 0;
        loop {
            let Some(leased) = self.context.store.lease_next_outbox(now_ms()).await? else {
                return Ok(OutboxDrainSummary {
                    count,
                    result_class: if count == 0 { "empty" } else { "drained" },
                });
            };
            count = count.saturating_add(1);
            let item = leased.item.clone();

            if let Some((destination, frame)) = outbound_frame(&item.effect) {
                match self
                    .start_outbound_send(&item, leased.lease_id, destination, frame)
                    .await
                {
                    Ok(()) => {
                        // Keep the lease in flight until the receiver has
                        // acknowledged durable responsibility. The actor
                        // must continue selecting commands while that task
                        // waits, or bilateral sends deadlock.
                        return Ok(OutboxDrainSummary {
                            count,
                            result_class: "send_started",
                        });
                    }
                    Err(error) => {
                        let reason = error.to_string();
                        self.context
                            .store
                            .retry_outbox(item.outbox_id, leased.lease_id, now_ms(), reason)
                            .await?;
                        if matches!(error, OutboxDeliveryError::Retryable(_)) {
                            return Ok(OutboxDrainSummary {
                                count,
                                result_class: "retry_scheduled",
                            });
                        }
                        return Err(error.into_runtime());
                    }
                }
            }

            let result = self.deliver_effect(&item.effect).await;
            match result {
                Ok(()) => {
                    self.context
                        .store
                        .acknowledge_outbox(item.outbox_id, leased.lease_id)
                        .await?;
                }
                Err(error) => {
                    let reason = error.to_string();
                    self.context
                        .store
                        .retry_outbox(item.outbox_id, leased.lease_id, now_ms(), reason)
                        .await?;
                    if matches!(error, OutboxDeliveryError::Retryable(_)) {
                        return Ok(OutboxDrainSummary {
                            count,
                            result_class: "retry_scheduled",
                        });
                    }
                    return Err(error.into_runtime());
                }
            }
        }
    }

    /// Start one remote frame delivery without blocking the serialized actor
    /// command future on its transport responsibility receipt.
    async fn start_outbound_send(
        &mut self,
        item: &OutboxItem,
        lease_id: arena0_store::LeaseId,
        destination: arena0_protocol::PeerId,
        frame: ExecFrame,
    ) -> Result<(), OutboxDeliveryError> {
        let handle = self.send_handle(destination).await?;
        let task = tokio::spawn(async move { handle.send_exec(&frame).await });
        self.inflight_send = Some(InflightSend {
            destination,
            outbox_id: item.outbox_id,
            lease_id,
            task: Some(task),
        });
        Ok(())
    }

    /// Settle the remote send selected as ready by the actor loop.
    pub(super) async fn settle_inflight_send(
        &mut self,
        joined: Result<Result<(), TransportError>, tokio::task::JoinError>,
    ) -> Result<(), ExecError> {
        let mut inflight = self
            .inflight_send
            .take()
            .expect("in-flight send exists after task completion");
        let _task = inflight
            .task
            .take()
            .expect("in-flight send task exists after task completion");
        let result = match joined {
            Ok(result) => result.map_err(classify_transport_error),
            Err(error) => Err(OutboxDeliveryError::Retryable(ExecError::Unavailable(
                format!("outbound execution send task failed: {error}"),
            ))),
        };
        match result {
            Ok(()) => {
                self.context
                    .store
                    .acknowledge_outbox(inflight.outbox_id, inflight.lease_id)
                    .await?;
                Ok(())
            }
            Err(error) => {
                // A failed send cannot safely reuse the stream: the remote
                // may have consumed the packet before the connection failed.
                self.send_streams.remove(&inflight.destination);
                let reason = error.to_string();
                self.context
                    .store
                    .retry_outbox(inflight.outbox_id, inflight.lease_id, now_ms(), reason)
                    .await?;
                if matches!(error, OutboxDeliveryError::Retryable(_)) {
                    Ok(())
                } else {
                    Err(error.into_runtime())
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) async fn deliver_effect_for_test(
        &mut self,
        effect: &DurableEffect,
    ) -> Result<(), ExecError> {
        self.deliver_effect(effect)
            .await
            .map_err(OutboxDeliveryError::into_runtime)
    }

    async fn deliver_effect(&mut self, effect: &DurableEffect) -> Result<(), OutboxDeliveryError> {
        if matches!(
            effect,
            DurableEffect::RequestCallout { .. }
                | DurableEffect::RequestSignature { .. }
                | DurableEffect::RetryInput { .. }
                | DurableEffect::ApplyBroadcast { .. }
                | DurableEffect::RequestStepSignature { .. }
                | DurableEffect::RequestTerminalSignature { .. }
        ) && self.load_state().await?.status().terminal_cause().is_some()
        {
            // The durable stop supersedes guest continuations. Delivery can
            // acknowledge them without invoking a guest or emitting a callout.
            return Ok(());
        }
        match effect {
            DurableEffect::SendBroadcast { destination, frame } => {
                let wire = ExecFrame::Message {
                    message_id: frame.message_id(),
                    seq: frame.sequence(),
                    prestate: frame.pre_state(),
                    data: frame.data().to_vec(),
                    witness: frame.witness(),
                };
                self.send_frame(*destination, wire).await
            }
            DurableEffect::ApplyBroadcast { frame } => {
                let state = self.load_state().await?;
                let already_staged = state.pending_shared().is_some_and(|proposal| {
                    matches!(
                        &proposal.entry().event,
                        PublicEvent::MessageReceived {
                            message_id,
                            from,
                            position,
                            pre_state,
                            msg,
                        } if *message_id == frame.message_id()
                            && *from == self.context.identity.peer_id()
                            && *position == frame.sequence()
                            && *pre_state == frame.pre_state()
                            && msg == frame.data()
                            && proposal.entry().witness == Some(frame.witness())
                    )
                });
                if !already_staged {
                    let applied = self
                        .apply_message(self.context.identity.peer_id(), frame.clone(), None)
                        .await?;
                    if !applied {
                        // A crash can occur after the local shared transition
                        // is durable but before this outbox lease is
                        // acknowledged. The durable public cursor then proves
                        // that this causal position has already advanced;
                        // replaying the effect is an idempotent delivery, not
                        // a guest rejection. At the original position,
                        // however, a false result is a real rejection and must
                        // keep the outbox unacknowledged.
                        let state = self.load_state().await?;
                        if state.status().is_terminal() {
                            // A deterministic local rejection can cause the
                            // actor to persist a terminal failure before this
                            // old lease is revisited. The failed broadcast is
                            // then superseded by that terminal boundary and
                            // must not block the causal abort outbox forever.
                            return Ok(());
                        }
                        if state.public().next_step() <= frame.sequence() {
                            return Err(ExecError::InvalidState(
                                "local broadcast was rejected by the shared guest handler".into(),
                            )
                            .into());
                        }
                    }
                }
                self.ensure_step_signature().await.map_err(Into::into)
            }
            DurableEffect::Notify { frame_id, payload } => self
                .messages
                .send(SessionMessage::Notification {
                    frame_id: *frame_id,
                    payload: payload.clone(),
                })
                .await
                .map_err(|_| ExecError::Unavailable("message receiver closed".into()))
                .map_err(Into::into),
            DurableEffect::RequestCallout { pending, context } => {
                let arena0_protocol::PendingOperation::Callout { callout_index } =
                    pending.operation
                else {
                    return Err(ExecError::InvalidState(
                        "callout request has no schema index".into(),
                    )
                    .into());
                };
                self.messages
                    .send(SessionMessage::CalloutRequested {
                        pending_id: pending.id,
                        callout_index,
                        context: context.clone(),
                        expected_type: pending.expected_type.clone(),
                    })
                    .await
                    .map_err(|_| ExecError::Unavailable("message receiver closed".into()))
                    .map_err(Into::into)
            }
            DurableEffect::RequestSignature { pending, data } => self
                .sign_and_resume(pending.id, data)
                .await
                .map_err(Into::into),
            DurableEffect::RetryInput { pending_id, reason } => {
                let state = self.load_state().await?;
                let Some((pending, _)) = state.status().pending() else {
                    return Err(ExecError::InvalidState(
                        "retry effect has no pending callout".into(),
                    )
                    .into());
                };
                let arena0_protocol::PendingOperation::Callout { callout_index } =
                    pending.operation
                else {
                    return Err(ExecError::InvalidState(
                        "retry effect does not name a callout".into(),
                    )
                    .into());
                };
                if pending.id != *pending_id {
                    return Err(ExecError::InvalidState(
                        "retry effect does not match pending callout".into(),
                    )
                    .into());
                }
                self.messages
                    .send(SessionMessage::CalloutRequested {
                        pending_id: pending.id,
                        callout_index,
                        context: Vec::new(),
                        expected_type: pending.expected_type.clone(),
                    })
                    .await
                    .map_err(|_| ExecError::Unavailable("message receiver closed".into()))?;
                let _ = reason;
                Ok(())
            }
            DurableEffect::PublishReceipt { receipt } => {
                let receipt = receipt.as_ref().clone();
                self.messages
                    .send(SessionMessage::ReceiptPublished {
                        receipt: receipt.clone(),
                    })
                    .await
                    .map_err(|_| ExecError::Unavailable("message receiver closed".into()))?;
                match receipt.body().termination() {
                    arena0_protocol::ReceiptTermination::Completed { .. } => {
                        let result_json = self
                            .load_state()
                            .await?
                            .terminal_outcome_json()
                            .map(ToOwned::to_owned);
                        self.messages
                            .send(SessionMessage::Completed {
                                result: receipt.body().outcome().to_vec(),
                                result_json,
                            })
                            .await
                            .map_err(|_| {
                                ExecError::Unavailable("message receiver closed".into())
                            })?;
                    }
                    arena0_protocol::ReceiptTermination::Stopped { cause } => {
                        let (step, reason) = match cause {
                            arena0_protocol::StopCause::Authenticated(occurrence) => (
                                occurrence.coordinate().next_step(),
                                occurrence.reason().to_owned(),
                            ),
                            arena0_protocol::StopCause::Shared {
                                commitment, reason, ..
                            } => (commitment.step, reason.clone()),
                        };
                        let message = match cause.kind() {
                            arena0_protocol::AbortKind::Abort => {
                                SessionMessage::Aborted { step, reason }
                            }
                            arena0_protocol::AbortKind::Fail => SessionMessage::Failed { reason },
                        };
                        self.messages.send(message).await.map_err(|_| {
                            ExecError::Unavailable("message receiver closed".into())
                        })?;
                    }
                }
                Ok(())
            }
            DurableEffect::SendAbort {
                destination,
                occurrence,
            } => {
                self.send_frame(
                    *destination,
                    ExecFrame::Abort {
                        occurrence: occurrence.clone(),
                    },
                )
                .await
            }
            DurableEffect::RequestStepSignature { .. } => {
                self.ensure_step_signature().await.map_err(Into::into)
            }
            DurableEffect::PublishStepSignature {
                destination,
                commitment,
                signature,
            } => {
                self.send_frame(
                    *destination,
                    ExecFrame::StepSignature {
                        commitment: commitment.clone(),
                        signature: *signature,
                    },
                )
                .await
            }
            DurableEffect::RequestTerminalSignature { .. } => {
                self.ensure_terminal_signature().await.map_err(Into::into)
            }
            DurableEffect::PublishTerminalSignature {
                destination,
                commitment,
                signature,
            } => {
                self.send_frame(
                    *destination,
                    ExecFrame::End {
                        commitment: commitment.clone(),
                        signature: *signature,
                    },
                )
                .await
            }
        }
    }

    async fn send_frame(
        &mut self,
        destination: arena0_protocol::PeerId,
        frame: ExecFrame,
    ) -> Result<(), OutboxDeliveryError> {
        let handle = self.send_handle(destination).await?;
        if let Err(error) = handle.send_exec(&frame).await {
            self.send_streams.remove(&destination);
            return Err(classify_transport_error(error));
        }
        Ok(())
    }

    async fn send_handle(
        &mut self,
        destination: arena0_protocol::PeerId,
    ) -> Result<arena0_transport::SendHandle, OutboxDeliveryError> {
        if let Some(handle) = self.send_streams.get(&destination) {
            return Ok(handle.clone());
        }
        let handle = self
            .context
            .transport
            .open_exec(&destination, self.context.activation.session_hash())
            .await
            .map_err(classify_transport_error)?;
        self.send_streams.insert(destination, handle.clone());
        Ok(handle)
    }
}

fn record_outbox_drain(
    started: Instant,
    exec_id: arena0_protocol::ExecId,
    session_id: arena0_protocol::SessionHash,
    result: &Result<OutboxDrainSummary, ExecError>,
) {
    if matches!(result, Ok(summary) if summary.count == 0) {
        return;
    }
    let (count, success, result_class) = match result {
        Ok(summary) => (summary.count, true, summary.result_class),
        Err(_) => (0, false, "error"),
    };
    let elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    tracing::debug!(
        target: PERFORMANCE_TARGET,
        operation = "outbox_drain",
        ?exec_id,
        ?session_id,
        count,
        success,
        result_class,
        elapsed_us,
    );
}

/// Convert a remote durable effect to the exact typed execution frame that
/// must cross the transport boundary. Local effects remain on the actor path.
fn outbound_frame(effect: &DurableEffect) -> Option<(arena0_protocol::PeerId, ExecFrame)> {
    match effect {
        DurableEffect::SendBroadcast { destination, frame } => Some((
            *destination,
            ExecFrame::Message {
                message_id: frame.message_id(),
                seq: frame.sequence(),
                prestate: frame.pre_state(),
                data: frame.data().to_vec(),
                witness: frame.witness(),
            },
        )),
        DurableEffect::SendAbort {
            destination,
            occurrence,
        } => Some((
            *destination,
            ExecFrame::Abort {
                occurrence: occurrence.clone(),
            },
        )),
        DurableEffect::PublishStepSignature {
            destination,
            commitment,
            signature,
        } => Some((
            *destination,
            ExecFrame::StepSignature {
                commitment: commitment.clone(),
                signature: *signature,
            },
        )),
        DurableEffect::PublishTerminalSignature {
            destination,
            commitment,
            signature,
        } => Some((
            *destination,
            ExecFrame::End {
                commitment: commitment.clone(),
                signature: *signature,
            },
        )),
        DurableEffect::ApplyBroadcast { .. }
        | DurableEffect::Notify { .. }
        | DurableEffect::RequestCallout { .. }
        | DurableEffect::RequestSignature { .. }
        | DurableEffect::RetryInput { .. }
        | DurableEffect::PublishReceipt { .. }
        | DurableEffect::RequestStepSignature { .. }
        | DurableEffect::RequestTerminalSignature { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use super::{OutboxDrainSummary, record_outbox_drain};

    #[test]
    fn performance_outbox_record_contains_only_safe_fields() {
        let output = SharedWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(output.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            record_outbox_drain(
                Instant::now(),
                arena0_protocol::ExecId([1; 32]),
                arena0_protocol::SessionHash([2; 32]),
                &Ok(OutboxDrainSummary {
                    count: 4,
                    result_class: "drained",
                }),
            );
        });

        let line = output.line();
        let json: serde_json::Value = serde_json::from_str(&line).expect("JSON trace line");
        assert_eq!(json["target"], "arena0::performance");
        let fields = json["fields"].as_object().expect("structured fields");
        let allowed = [
            "operation",
            "exec_id",
            "session_id",
            "count",
            "success",
            "result_class",
            "elapsed_us",
        ];
        assert!(fields.keys().all(|field| allowed.contains(&field.as_str())));
        assert_eq!(fields["operation"], "outbox_drain");
        assert_eq!(fields["count"], 4);
        assert_eq!(fields["success"], true);
        assert!(!fields.contains_key("params"));
        assert!(!fields.contains_key("outcome"));
        assert!(!fields.contains_key("payload"));
        assert!(!fields.contains_key("signature"));
        assert!(!fields.contains_key("sql"));
    }

    #[test]
    fn performance_outbox_record_skips_empty_drain() {
        let output = SharedWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(output.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            record_outbox_drain(
                Instant::now(),
                arena0_protocol::ExecId([1; 32]),
                arena0_protocol::SessionHash([2; 32]),
                &Ok(OutboxDrainSummary {
                    count: 0,
                    result_class: "empty",
                }),
            );
        });

        assert!(output.line().is_empty());
    }

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl SharedWriter {
        fn line(&self) -> String {
            String::from_utf8(self.0.lock().expect("writer lock").clone())
                .expect("trace output is UTF-8")
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedWriter {
        type Writer = SharedWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("writer lock").extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
