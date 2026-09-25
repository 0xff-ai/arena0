//! State-driven delivery. The actor authenticates and classifies frames from
//! its in-memory state; only a committed apply or a stale/duplicate decision
//! acknowledges a delivery. Restart resends the retained protocol evidence.

use std::collections::HashSet;
use std::time::Duration;

use arena0_protocol::{ExecFrame, ParticipantStepSignature, PeerId, ProtocolError};
use arena0_store::Change;
use arena0_transport::{ExecDelivery, ExecDeliveryRejection, SendHandle, TransportError};
use tokio::time::Instant;

use super::{ExecutionActor, PROGRESS_INTERVAL};
use crate::context::ExecError;

const SEND_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(super) struct SendLane {
    handle: Option<SendHandle>,
    acked: HashSet<[u8; 32]>,
    /// Refusals may suppress retries only while a frame is non-final. Keeping
    /// them separate prevents a later stop from counting a refusal as an ack.
    rejected: HashSet<[u8; 32]>,
    busy: bool,
    suppressed: bool,
    retry_at: Option<Instant>,
}

pub(super) struct SendResult {
    peer: PeerId,
    digest: [u8; 32],
    handle: Option<SendHandle>,
    result: Result<(), TransportError>,
}

impl ExecutionActor {
    pub(super) async fn inbound(&mut self, delivery: ExecDelivery) -> Result<(), ExecError> {
        let source = delivery.source();
        let frame = delivery.frame().clone();
        let result = self.accept_frame(source, frame).await;
        match result {
            Ok(None) => {
                let _ = delivery.acknowledge();
            }
            Ok(Some(rejection)) => {
                let _ = delivery.reject(rejection);
            }
            Err(error @ ExecError::Diverged(_)) => {
                // A divergent dispatch is applied as a durable local failure.
                // If persistence fails, dropping the delivery keeps the sender
                // responsible for retrying it.
                if !self.fail_terminal(error.clone()).await {
                    return Err(error);
                }
                let _ = delivery.acknowledge();
            }
            Err(error) => return Err(error),
        }
        self.progress().await
    }

    pub(super) async fn accept_frame(
        &mut self,
        source: PeerId,
        frame: ExecFrame,
    ) -> Result<Option<ExecDeliveryRejection>, ExecError> {
        use ExecDeliveryRejection::{Conflict, NotYet, Rejected};
        if !self
            .state
            .binding()
            .activation()
            .tickets()
            .iter()
            .any(|ticket| ticket.data.signer == source)
        {
            return Ok(Some(Rejected));
        }
        // Authenticate terminal evidence before comparing conclusions. Adopted
        // aborts retain their original signer, independently of the forwarding peer.
        match &frame {
            ExecFrame::StepCertificate { certificate }
                if certificate.verify(self.state.binding()).is_err() =>
            {
                return Ok(Some(Rejected));
            }
            ExecFrame::Abort { occurrence }
                if occurrence.session_id() != self.state.binding().session_id()
                    || !self
                        .state
                        .binding()
                        .activation()
                        .tickets()
                        .iter()
                        .any(|t| t.data.signer == occurrence.sender())
                    || !occurrence.verify_signature().unwrap_or(false) =>
            {
                return Ok(Some(Rejected));
            }
            ExecFrame::StepSignature { commitment, .. }
                if commitment.session_id != self.state.binding().session_id() =>
            {
                return Ok(Some(Rejected));
            }
            _ => {}
        }
        if self.state.status().is_terminal() {
            match self.state.end_conclusion_matches(&frame) {
                arena0_protocol::EndMatch::Same => self.confirm_peer(source).await?,
                arena0_protocol::EndMatch::Different => {
                    tracing::error!(exec_id = %self.context.exec_id, peer = %source, "peer terminal conclusion differs");
                    self.send_lanes.entry(source).or_default().suppressed = true;
                    return Ok(Some(Conflict));
                }
                arena0_protocol::EndMatch::NotTerminalEvidence => {}
            }
            return Ok(None);
        }
        let step = self.state.agreed_step();
        match frame {
            frame @ ExecFrame::Message { .. } => {
                let ExecFrame::Message {
                    ref commitment,
                    ref data,
                } = frame
                else {
                    unreachable!()
                };
                if commitment.step < step {
                    return Ok(None);
                }
                if commitment.step > step {
                    return Ok(Some(NotYet));
                }
                if commitment.session_id != self.state.binding().session_id()
                    || commitment.pre_state != self.state.agreed_state()
                    || commitment.link != self.state.agreed_link()
                    || !self.writer_is(source, &self.state, &self.ensemble())?
                {
                    return Ok(Some(Rejected));
                }
                if let Some(proposal) = self.state.pending_shared() {
                    return Ok(
                        if proposal.commitment() == commitment
                            && matches!(&proposal.entry().event,
                                arena0_protocol::StepEvent::Message { from, data: staged }
                                    if *from == source && staged == data)
                        {
                            None
                        } else {
                            Some(Conflict)
                        },
                    );
                }
                self.apply_message(source, frame).await?;
            }
            ExecFrame::StepSignature {
                commitment,
                signature,
            } => {
                if commitment.step < step {
                    return Ok(None);
                }
                if commitment.step > step {
                    return Ok(Some(NotYet));
                }
                let Some(proposal) = self.state.pending_shared() else {
                    return Ok(Some(NotYet));
                };
                if proposal.commitment() != &commitment {
                    return Ok(Some(Rejected));
                }
                let signature = ParticipantStepSignature::new(source, commitment.step, signature);
                if proposal.signatures().contains(&signature) {
                    return Ok(None);
                }
                let mut next = self.state.clone();
                let Ok(certified) = next.add_step_signature(signature) else {
                    return Ok(Some(Rejected));
                };
                let agreed = certified.as_ref().map(|proposal| proposal.entry().step);
                self.persist(next, Change::StepSignature { certified })
                    .await?;
                self.reconcile_resident()?;
                self.emit_trace_appended(agreed).await;
            }
            ExecFrame::StepCertificate { certificate } => {
                if certificate.commitment().step < step {
                    return Ok(None);
                }
                let Some(proposal) = self.state.pending_shared() else {
                    return Ok(Some(NotYet));
                };
                if proposal.commitment() != certificate.commitment() {
                    return Err(ExecError::DeliveryInvariant(
                        "verified certificate contradicts the staged proposal",
                    ));
                }
                let mut next = self.state.clone();
                let certified = next.certify_step(certificate).map_err(|_| {
                    ExecError::DeliveryInvariant("verified certificate could not be committed")
                })?;
                let agreed = certified.entry().step;
                self.persist(
                    next,
                    Change::StepSignature {
                        certified: Some(certified),
                    },
                )
                .await?;
                self.reconcile_resident()?;
                self.emit_trace_appended(Some(agreed)).await;
            }
            ExecFrame::Abort { occurrence } => {
                if occurrence.coordinate().next_step() < step {
                    return Ok(None);
                }
                if occurrence.coordinate().next_step() > step {
                    return Ok(Some(NotYet));
                }
                let mut next = self.state.clone();
                match next.stop(occurrence) {
                    Ok(()) => {}
                    Err(ProtocolError::SharedProposalSigned) => return Ok(Some(NotYet)),
                    Err(ProtocolError::InvalidAbortCoordinate) => {
                        tracing::warn!(exec_id = %self.context.exec_id, peer = %source, "authenticated abort contradicts the agreed cursor");
                        return Ok(Some(Conflict));
                    }
                    Err(_) => return Ok(Some(Rejected)),
                }
                self.persist(next, Change::Stop).await?;
            }
        }
        Ok(None)
    }

    /// Start at most one send per peer. Opening a stream and waiting for its
    /// acknowledgement share a deadline. JoinSet owns cancellation when the
    /// actor exits, and each task owns its stream until settlement.
    pub(super) fn deliver_frames(&mut self) -> Result<bool, ExecError> {
        let ending = self.state.status().is_terminal();
        let frames = if ending {
            self.state.terminal_evidence().into_iter().collect()
        } else {
            self.state.current_frames(self.context.producer)
        };
        let frames = frames
            .into_iter()
            .map(|frame| Ok((frame_digest(&frame)?, frame)))
            .collect::<Result<Vec<_>, ExecError>>()?;
        let mut delivered = true;
        for ticket in self.context.activation.tickets() {
            let peer = ticket.data.signer;
            if peer == self.context.producer {
                continue;
            }
            if ending
                && !self
                    .state
                    .end_phase()
                    .unconfirmed()
                    .is_some_and(|peers| peers.contains(&peer))
            {
                continue;
            }
            let lane = self.send_lanes.entry(peer).or_default();
            lane.acked
                .retain(|digest| frames.iter().any(|(current, _)| current == digest));
            lane.rejected
                .retain(|digest| frames.iter().any(|(current, _)| current == digest));
            if lane.suppressed {
                delivered = false;
                continue;
            }
            let Some((digest, frame)) = frames.iter().find(|(digest, _)| {
                !lane.acked.contains(digest) && (ending || !lane.rejected.contains(digest))
            }) else {
                continue;
            };
            delivered = false;
            if lane.busy
                || lane
                    .retry_at
                    .is_some_and(|deadline| Instant::now() < deadline)
            {
                continue;
            }
            lane.busy = true;
            let handle = lane.handle.take();
            let transport = self.context.transport.clone();
            let session = self.context.activation.session_hash();
            let digest = *digest;
            let frame = frame.clone();
            self.send_tasks.spawn(async move {
                let operation = async move {
                    let handle = match handle {
                        Some(handle) => handle,
                        None => transport.open_exec(&peer, session).await?,
                    };
                    let result = handle.send_exec(&frame).await;
                    Ok::<_, TransportError>((handle, result))
                };
                let (handle, result) = match tokio::time::timeout(SEND_DEADLINE, operation).await {
                    Ok(Ok((handle, result))) => (Some(handle), result),
                    Ok(Err(error)) => (None, Err(error)),
                    Err(_) => (None, Err(TransportError::Timeout(5000))),
                };
                SendResult {
                    peer,
                    digest,
                    handle,
                    result,
                }
            });
        }
        Ok(delivered)
    }

    async fn confirm_peer(&mut self, peer: PeerId) -> Result<(), ExecError> {
        let mut next = self.state.clone();
        if next.confirm_end(peer)? {
            self.persist(next, Change::End).await?;
        }
        Ok(())
    }

    pub(super) async fn settle_send(&mut self, result: SendResult) -> Result<(), ExecError> {
        let SendResult {
            peer,
            digest,
            handle,
            result,
        } = result;
        let final_frame = self
            .state
            .terminal_evidence()
            .as_ref()
            .is_some_and(|frame| frame_digest(frame).is_ok_and(|current| current == digest));
        let lane = self.send_lanes.entry(peer).or_default();
        lane.busy = false;
        lane.retry_at = None;
        match result {
            Ok(()) => {
                lane.acked.insert(digest);
                lane.handle = handle;
                if final_frame {
                    self.confirm_peer(peer).await?;
                }
            }
            Err(TransportError::ExecRejected) => {
                if final_frame {
                    tracing::error!(exec_id = %self.context.exec_id, %peer, "peer rejected terminal evidence");
                    lane.suppressed = true;
                    return Ok(());
                }
                tracing::warn!(exec_id = %self.context.exec_id, %peer, "execution frame rejected");
                lane.rejected.insert(digest);
                lane.handle = handle;
            }
            Err(TransportError::ExecConflict) => {
                // A receipt is not evidence of divergence. Only verified
                // frames on the receive path can change our conclusion.
                tracing::error!(exec_id = %self.context.exec_id, %peer, "peer reported conflicting execution evidence");
                lane.suppressed = true;
            }
            Err(TransportError::ExecNotYet) => {
                lane.handle = handle;
                lane.retry_at = Some(Instant::now() + PROGRESS_INTERVAL);
            }
            Err(_) => {
                lane.retry_at = Some(Instant::now() + PROGRESS_INTERVAL);
            }
        }
        Ok(())
    }
}

fn frame_digest(frame: &ExecFrame) -> Result<[u8; 32], ExecError> {
    let bytes = borsh::to_vec(frame).map_err(|error| ExecError::InvalidState(error.to_string()))?;
    Ok(*blake3::hash(&bytes).as_bytes())
}
