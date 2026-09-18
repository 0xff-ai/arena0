//! Durable outbox leasing and delivery.
//!
//! The store keeps two payload classes: guest [`Effect`] values and protocol
//! [`ExecFrame`] values. They have different delivery obligations and are
//! decoded only after the lease crosses the durable boundary. The store
//! expands a protocol frame into one row per remote participant, so each lease
//! has one transport responsibility. The actor never feeds its own broadcast
//! frame back into `MessageReceived`.

use std::time::Instant;

use crate::context::ExecError;
use arena0_protocol::execution::GuestSignData;
use arena0_protocol::{Effect, ExecFrame, PendingOperation, ReceiptWork};
use arena0_store::{OutboxItem, OutboxPayloadKind};
use arena0_transport::TransportError;

use super::{ExecutionActor, InflightSend, callout_requested, now_ms};

const PERFORMANCE_TARGET: &str = "arena0::performance";

#[derive(Debug, Clone, Copy)]
pub(super) struct OutboxDrainSummary {
    count: usize,
    result_class: &'static str,
    pub(super) sign_consumed: bool,
}

/// A leased delivery can fail without invalidating the execution. Transport
/// outages belong to durable retry; malformed durable payloads and a closed
/// observer channel are actor failures.
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

impl From<OutboxDeliveryError> for ExecError {
    fn from(error: OutboxDeliveryError) -> Self {
        error.into_runtime()
    }
}

impl From<arena0_store::StoreError> for OutboxDeliveryError {
    fn from(error: arena0_store::StoreError) -> Self {
        Self::Retryable(ExecError::from(error))
    }
}

impl From<arena0_protocol::ProtocolError> for OutboxDeliveryError {
    fn from(error: arena0_protocol::ProtocolError) -> Self {
        Self::Terminal(ExecError::from(error))
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
    /// Drain durable work and report whether a guest Sign continuation was
    /// consumed. The actor progress loop uses that bit to immediately revisit
    /// the state machine: a Signed dispatch can expose a proposal or another
    /// effect, but re-entering `progress` from Sign delivery would make an
    /// infinitely-sized async future.
    pub(super) async fn drain_outbox_report(&mut self) -> Result<OutboxDrainSummary, ExecError> {
        if self.inflight_send.is_some() {
            return Ok(OutboxDrainSummary {
                count: 0,
                result_class: "send_inflight",
                sign_consumed: false,
            });
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
        result
    }

    async fn drain_outbox_inner(&mut self) -> Result<OutboxDrainSummary, ExecError> {
        let mut count = 0;
        let mut sign_consumed = false;
        loop {
            let Some(leased) = self.context.store.lease_next_outbox(now_ms()).await? else {
                return Ok(OutboxDrainSummary {
                    count,
                    result_class: if count == 0 { "empty" } else { "drained" },
                    sign_consumed,
                });
            };
            count = count.saturating_add(1);
            let item = leased.item;
            let result = match item.payload_kind {
                OutboxPayloadKind::Frame => {
                    let frame = decode_frame(&item.payload)?;
                    let destination = item.destination.ok_or_else(|| {
                        OutboxDeliveryError::Terminal(ExecError::InvalidState(
                            "protocol frame outbox row has no destination".into(),
                        ))
                    })?;
                    match self
                        .start_outbound_send(item.outbox_id, leased.lease_id, destination, frame)
                        .await
                    {
                        Ok(()) => {
                            // Keep the lease in flight until this receiver
                            // acknowledges durable responsibility. The store
                            // creates one row per remote destination.
                            return Ok(OutboxDrainSummary {
                                count,
                                result_class: "send_started",
                                sign_consumed,
                            });
                        }
                        Err(error) => Err(error),
                    }
                }
                OutboxPayloadKind::Effect => {
                    let effect = decode_effect(&item.payload)?;
                    self.deliver_effect(&effect, &item).await
                }
            };

            match result {
                Ok(effect_consumed_sign) => {
                    sign_consumed |= effect_consumed_sign;
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
                            sign_consumed,
                        });
                    }
                    return Err(error.into_runtime());
                }
            }
        }
    }

    /// Start one remote frame delivery without blocking the serialized actor
    /// command future. The store creates independent rows for each remote
    /// destination, so one lease has exactly one transport responsibility.
    async fn start_outbound_send(
        &mut self,
        outbox_id: arena0_store::OutboxId,
        lease_id: arena0_store::LeaseId,
        destination: arena0_protocol::PeerId,
        frame: ExecFrame,
    ) -> Result<(), OutboxDeliveryError> {
        let handle = self.send_handle(destination).await?;
        let task_frame = frame.clone();
        let task = tokio::spawn(async move { handle.send_exec(&task_frame).await });
        self.inflight_send = Some(InflightSend {
            destination,
            outbox_id,
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

    async fn deliver_effect(
        &mut self,
        effect: &Effect,
        item: &OutboxItem,
    ) -> Result<bool, OutboxDeliveryError> {
        if matches!(
            effect,
            Effect::Callout { .. } | Effect::Sign { .. } | Effect::RetryInput { .. }
        ) {
            let state = self.load_state().await?;
            if !matches!(state.status().receipt_work(), ReceiptWork::NotTerminal) {
                // Every terminal boundary supersedes an older guest
                // continuation. This includes successful terminal-proof
                // collection, not only an authenticated stop.
                return Ok(false);
            }
        }
        match effect {
            Effect::Callout {
                callout_index,
                context,
                expected_type,
                ..
            } => {
                let state = self.load_state().await?;
                let Some(pending) = state.status().pending() else {
                    return Err(ExecError::InvalidState(
                        "callout effect has no pending continuation".into(),
                    )
                    .into());
                };
                if pending.operation
                    != (PendingOperation::Callout {
                        callout_index: *callout_index,
                    })
                {
                    return Err(ExecError::InvalidState(
                        "callout effect does not match pending continuation".into(),
                    )
                    .into());
                }
                self.messages
                    .send(callout_requested(
                        pending.id,
                        *callout_index,
                        context.clone(),
                        expected_type
                            .clone()
                            .or_else(|| pending.expected_type.clone()),
                    ))
                    .await
                    .map_err(|_| ExecError::Unavailable("message receiver closed".into()))?;
                Ok(false)
            }
            Effect::Sign {
                scheme,
                data,
                continuation_tag,
                ..
            } => {
                let pending_id = arena0_protocol::pending_id(
                    self.context.exec_id,
                    item.event_position,
                    item.ordinal,
                );
                let data = GuestSignData::new(
                    self.context.activation.session_hash(),
                    self.context.program.program().hash(),
                    self.context.exec_id,
                    item.event_position,
                    item.ordinal,
                    *scheme,
                    data.clone(),
                )?;
                if !self
                    .sign_and_resume(pending_id, &data, *continuation_tag)
                    .await?
                {
                    // A signing request is acknowledged only after its
                    // exact continuation was consumed. A frozen proposal or
                    // a guest rejection leaves the same outbox row retryable.
                    return Err(OutboxDeliveryError::Retryable(ExecError::Unavailable(
                        "signature continuation was not consumed".into(),
                    )));
                }
                Ok(true)
            }
            Effect::RetryInput { .. } => {
                let state = self.load_state().await?;
                let Some(pending) = state.status().pending() else {
                    return Err(ExecError::InvalidState(
                        "retry effect has no pending continuation".into(),
                    )
                    .into());
                };
                let PendingOperation::Callout { callout_index } = pending.operation else {
                    return Err(ExecError::InvalidState(
                        "retry effect does not name a callout".into(),
                    )
                    .into());
                };
                let context = self
                    .context
                    .store
                    .pending_requests()
                    .await?
                    .into_iter()
                    .find_map(|request| match request {
                        arena0_store::PendingRequest::Callout {
                            pending_id,
                            context,
                            expected_type,
                            ..
                        } if pending_id == pending.id => Some((context, expected_type)),
                        _ => None,
                    });
                let (context, request_type) = context.unwrap_or_default();
                self.messages
                    .send(callout_requested(
                        pending.id,
                        callout_index,
                        context,
                        request_type.or_else(|| pending.expected_type.clone()),
                    ))
                    .await
                    .map_err(|_| ExecError::Unavailable("message receiver closed".into()))?;
                Ok(false)
            }
            _ => Err(ExecError::InvalidState(
                "non-deliverable effect reached the durable effect outbox".into(),
            )
            .into()),
        }
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

fn decode_effect(payload: &[u8]) -> Result<Effect, ExecError> {
    borsh::from_slice(payload)
        .map_err(|error| ExecError::InvalidState(format!("durable effect decode failed: {error}")))
}

fn decode_frame(payload: &[u8]) -> Result<ExecFrame, ExecError> {
    // The store retains protocol-domain frames as their canonical Borsh
    // value. The transport layer adds its own wire envelope only after this
    // actor has decoded the durable row.
    borsh::from_slice(payload)
        .map_err(|error| ExecError::InvalidState(format!("durable frame decode failed: {error}")))
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
